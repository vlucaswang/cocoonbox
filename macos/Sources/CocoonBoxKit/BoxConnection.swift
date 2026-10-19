import Foundation
import GRPCCore
import GRPCNIOTransportHTTP2TransportServices
import Synchronization

typealias BoxTransport = HTTP2ClientTransport.TransportServices
typealias BoxesClient = Cocoonbox_Control_V1_Boxes.Client<BoxTransport>
typealias SharesClient = Cocoonbox_Control_V1_Shares.Client<BoxTransport>
typealias FlowsClient = Cocoonbox_Control_V1_Flows.Client<BoxTransport>
typealias ConnectorsClient = Cocoonbox_Control_V1_Connectors.Client<BoxTransport>
typealias KeysClient = Cocoonbox_Control_V1_Keys.Client<BoxTransport>
typealias AccountClient = Cocoonbox_Control_V1_Account.Client<BoxTransport>
typealias AgentsClient = Cocoonbox_Agent_V1_Agents.Client<BoxTransport>
typealias SessionClient = Cocoonbox_Agent_V1_Session.Client<BoxTransport>

// BoxConnection owns the one HTTP/2 connection every call shares. It is the
// app's only way to the edge: its methods take and return proto messages and
// keep the gRPC clients to themselves.
public final class BoxConnection: Sendable {
    public let endpoint: Endpoint
    let boxes: BoxesClient
    let shares: SharesClient
    let flows: FlowsClient
    let connectors: ConnectorsClient
    let keys: KeysClient
    let account: AccountClient
    let agents: AgentsClient
    let session: SessionClient

    private let client: GRPCClient<BoxTransport>
    private let auth: AuthInterceptor
    // names is the name of every box id this connection has seen, so retarget can follow a deleted box's name.
    private let names = Mutex<[String: String]>([:])
    private let runner: Task<Void, Never>

    public init(endpoint: Endpoint, token: String) throws {
        guard endpoint.usesTLS || endpoint.isLoopback else {
            throw ConnectionError.plaintext(endpoint.host)
        }
        var config = BoxTransport.Config.defaults
        config.backoff = .init(initial: .milliseconds(250), max: .seconds(10), multiplier: 1.6, jitter: 0.2)
        config.connection.keepalive = .init(time: .seconds(20), timeout: .seconds(10), allowWithoutCalls: true)
        config.http2.authority = endpoint.host
        let transport = try BoxTransport(
            target: .dns(host: endpoint.host, port: endpoint.port),
            transportSecurity: endpoint.usesTLS ? .tls : .plaintext,
            config: config
        )
        let auth = AuthInterceptor(token: token)
        let client = GRPCClient(transport: transport, interceptors: [auth])
        self.endpoint = endpoint
        self.auth = auth
        self.client = client
        self.boxes = BoxesClient(wrapping: client)
        self.shares = SharesClient(wrapping: client)
        self.flows = FlowsClient(wrapping: client)
        self.connectors = ConnectorsClient(wrapping: client)
        self.keys = KeysClient(wrapping: client)
        self.account = AccountClient(wrapping: client)
        self.agents = AgentsClient(wrapping: client)
        self.session = SessionClient(wrapping: client)
        self.runner = Task { try? await client.runConnections() }
    }

    // boxID is the box of every agent call; setting an id whose name is unknown lists the boxes once to learn it.
    public var boxID: String? {
        get { auth.boxID.withLock { $0 } }
        set {
            auth.boxID.withLock { $0 = newValue }
            if let newValue, names.withLock({ $0[newValue] }) == nil {
                Task { _ = try? await self.listBoxes() }
            }
        }
    }

    // retarget follows boxID after the edge stopped resolving it: it keeps an id the user still has,
    // moves to the box that now has the deleted box's name, and throws BoxGone when no box has it.
    @discardableResult
    public func retarget() async throws -> String {
        guard let held = boxID else { throw BoxGone(name: "") }
        let name = names.withLock { $0[held] }
        let list = try await listBoxes()
        if list.contains(where: { $0.id == held }) { return held }
        guard let name, let renewed = list.first(where: { $0.name == name }) else { throw BoxGone(name: name ?? held) }
        return auth.boxID.withLock { box in
            if box == held { box = renewed.id }
            return box ?? renewed.id
        }
    }

    // isStale says whether error may mean the held box id names no box of the user's: NotFound, or PermissionDenied from an older api.
    public static func isStale(_ error: any Error) -> Bool {
        guard let rpc = error as? RPCError else { return false }
        return rpc.code == .notFound || rpc.code == .permissionDenied
    }

    public func close() {
        client.beginGracefulShutdown()
    }

    // MARK: Boxes

    public func firstBox() async throws -> Cocoonbox_Control_V1_Box? {
        try await listBoxes().first
    }

    // listBoxes returns the user's boxes, oldest first.
    public func listBoxes() async throws -> [Cocoonbox_Control_V1_Box] {
        let list = try await boxes.list(Cocoonbox_Control_V1_ListBoxesRequest()).boxes
        names.withLock { names in
            for box in list { names[box.id] = box.name }
        }
        return list
    }

    // watchBoxes runs Boxes.Watch until cancelled; onBoxes gets the user's boxes, oldest first, and the offered images,
    // at once, after each change by any client or the api, and again after each reconnect.
    public func watchBoxes(
        onBoxes: @escaping @Sendable ([Cocoonbox_Control_V1_Box], [String]) async -> Void,
        onDisconnect: @escaping @Sendable (any Error) async -> Void
    ) async {
        let boxes = boxes
        await EventFollower<Cocoonbox_Control_V1_ListBoxesResponse>.boxes().run(
            from: 0,
            subscribe: { _, deliver in
                try await boxes.watch(Cocoonbox_Control_V1_WatchBoxesRequest()) { response in
                    for try await list in response.messages {
                        await deliver(list)
                    }
                }
            },
            onEvent: { list in
                self.names.withLock { names in
                    for box in list.boxes { names[box.id] = box.name }
                }
                await onBoxes(list.boxes, list.offeredImages)
            },
            onResync: {},
            onDisconnect: onDisconnect
        )
    }

    // ensure wakes or creates boxID, or the user's first box when it is empty.
    public func ensure(boxID: String = "") async throws -> Cocoonbox_Control_V1_EnsureResponse {
        try seen(await boxes.ensure(.with { $0.boxID = boxID }))
    }

    // createBox runs image, an offered image or the family of one, else the default image.
    public func createBox(name: String, image: String = "") async throws -> Cocoonbox_Control_V1_EnsureResponse {
        try seen(await boxes.create(.with {
            $0.name = name
            $0.image = image
        }))
    }

    public func renameBox(boxID: String, name: String) async throws -> Cocoonbox_Control_V1_Box {
        let box = try await boxes.rename(.with {
            $0.boxID = boxID
            $0.name = name
        })
        names.withLock { $0[box.id] = box.name }
        return box
    }

    private func seen(_ response: Cocoonbox_Control_V1_EnsureResponse) -> Cocoonbox_Control_V1_EnsureResponse {
        names.withLock { $0[response.box.id] = response.box.name }
        return response
    }

    public func deleteBox(boxID: String) async throws -> Cocoonbox_Control_V1_Operation {
        try await boxes.delete(.with { $0.boxID = boxID })
    }

    public func hibernate(boxID: String) async throws -> Cocoonbox_Control_V1_Operation {
        try await boxes.hibernate(.with { $0.boxID = boxID })
    }

    public func saveState(boxID: String) async throws -> Cocoonbox_Control_V1_Operation {
        try await boxes.saveState(.with { $0.boxID = boxID })
    }

    public func rebase(boxID: String, image: String) async throws -> Cocoonbox_Control_V1_Operation {
        try await boxes.rebase(.with {
            $0.boxID = boxID
            $0.image = image
        })
    }

    // listSnapshots returns the box's kept snapshots, newest first.
    public func listSnapshots(boxID: String) async throws -> [Cocoonbox_Control_V1_Snapshot] {
        try await boxes.listSnapshots(.with { $0.boxID = boxID }).snapshots
    }

    // browseSnapshot lists one directory of a snapshot; an empty dir lists the roots.
    public func browseSnapshot(boxID: String, snapshotID: String, dir: String) async throws -> Cocoonbox_Control_V1_BrowseSnapshotResponse {
        try await boxes.browseSnapshot(.with {
            $0.boxID = boxID
            $0.snapshotID = snapshotID
            $0.dir = dir
        })
    }

    public func restorePaths(boxID: String, snapshotID: String, paths: [String]) async throws -> Cocoonbox_Control_V1_Operation {
        try await boxes.restorePaths(.with {
            $0.boxID = boxID
            $0.snapshotID = snapshotID
            $0.paths = paths
        })
    }

    // MARK: Wake schedules

    // setWakeSchedule wakes the box, when it is hibernated, a minute before each time of cron in tz and lets it
    // hibernate again after awakeForSeconds (0 is the api's default) unless a client reached it.
    public func setWakeSchedule(boxID: String, cron: String, tz: String, awakeForSeconds: UInt32 = 0) async throws -> Cocoonbox_Control_V1_WakeSchedule {
        try await boxes.setWakeSchedule(.with {
            $0.boxID = boxID
            $0.cron = cron
            $0.tz = tz
            $0.awakeForS = awakeForSeconds
        })
    }

    public func listWakeSchedules() async throws -> [Cocoonbox_Control_V1_WakeSchedule] {
        try await boxes.listWakeSchedules(Cocoonbox_Control_V1_ListWakeSchedulesRequest()).schedules
    }

    public func clearWakeSchedule(boxID: String) async throws {
        _ = try await boxes.clearWakeSchedule(.with { $0.boxID = boxID })
    }

    public func usage() async throws -> Cocoonbox_Control_V1_UsageResponse {
        try await boxes.usage(Cocoonbox_Control_V1_UsageRequest())
    }

    public func watchOperation(
        _ id: String,
        after offsetKey: String,
        deliver: @escaping @Sendable (Cocoonbox_Control_V1_OperationEvent) async -> Void
    ) async throws {
        try await boxes.watchOperation(.with {
            $0.operationID = id
            $0.fromOffsetKey = offsetKey
        }) { response in
            for try await event in response.messages {
                await deliver(event)
            }
        }
    }

    // MARK: Shares

    public func createShare(boxID: String, ttlSeconds: Int64, userNames: [String], write: Bool) async throws -> Cocoonbox_Control_V1_ShareSecret {
        try await shares.create(.with {
            $0.boxID = boxID
            $0.ttlSeconds = ttlSeconds
            $0.userNames = userNames
            $0.write = write
        })
    }

    public func listShares() async throws -> [Cocoonbox_Control_V1_Share] {
        try await shares.list(Cocoonbox_Control_V1_ListSharesRequest()).shares
    }

    public func setShareWrite(shareID: String, write: Bool) async throws -> Cocoonbox_Control_V1_Share {
        try await shares.setWrite(.with {
            $0.shareID = shareID
            $0.write = write
        })
    }

    public func regenerateShare(shareID: String) async throws -> Cocoonbox_Control_V1_ShareSecret {
        try await shares.regenerate(.with { $0.shareID = shareID })
    }

    public func revokeShare(shareID: String) async throws {
        _ = try await shares.revoke(.with { $0.shareID = shareID })
    }

    // MARK: Flows

    // listFlows returns the user's flow library without contents, newest first.
    public func listFlows() async throws -> Cocoonbox_Control_V1_ListFlowsResponse {
        try await flows.list(Cocoonbox_Control_V1_ListFlowsRequest())
    }

    // exportFlows returns the named flows with their contents; no names exports every flow.
    public func exportFlows(names: [String]) async throws -> [Cocoonbox_Control_V1_Flow] {
        try await flows.export(.with { $0.names = names }).flows
    }

    public func importFlows(_ items: [Cocoonbox_Control_V1_Flow]) async throws -> [Cocoonbox_Control_V1_Flow] {
        try await flows.save(.with { $0.flows = items }).flows
    }

    public func deleteFlows(names: [String]) async throws -> [String] {
        try await flows.delete(.with { $0.names = names }).deleted
    }

    // MARK: Keys

    public func addKey(_ request: Cocoonbox_Control_V1_AddKeyRequest) async throws -> Cocoonbox_Control_V1_Key {
        try await keys.add(request)
    }

    public func listKeys() async throws -> [Cocoonbox_Control_V1_Key] {
        try await keys.list(Cocoonbox_Control_V1_ListKeysRequest()).keys
    }

    public func rotateKey(name: String, value: String) async throws -> Cocoonbox_Control_V1_Key {
        try await keys.rotate(.with {
            $0.name = name
            $0.value = value
        })
    }

    public func deleteKey(name: String) async throws {
        _ = try await keys.delete(.with { $0.name = name })
    }

    // MARK: Account

    // listDevices returns the user's device tokens, newest first; current marks the one this connection signs in with.
    public func listDevices() async throws -> [Cocoonbox_Control_V1_Device] {
        try await account.listDevices(Cocoonbox_Control_V1_ListDevicesRequest()).devices
    }

    public func revokeDevice(id: String) async throws {
        _ = try await account.revokeDevice(.with { $0.deviceID = id })
    }

    // signOutEverywhere ends every sign-in of the user, this connection's token too.
    public func signOutEverywhere() async throws {
        _ = try await account.signOutEverywhere(Cocoonbox_Control_V1_SignOutEverywhereRequest())
    }

    // MARK: Connectors

    // catalog lists the connectors the api offers; `custom` connects any MCP server by URL.
    public func catalog() async throws -> [Cocoonbox_Control_V1_CatalogEntry] {
        try await connectors.catalog(Cocoonbox_Control_V1_CatalogRequest()).entries
    }

    // listConnectors returns the user's connected accounts; withTools lists each one's tools from its MCP server.
    public func listConnectors(withTools: Bool = false) async throws -> [Cocoonbox_Control_V1_Connector] {
        try await connectors.list(.with { $0.withTools = withTools }).connectors
    }

    // disconnect revokes the grant at the vendor where it can and deletes it; it returns whether the vendor revoked it.
    @discardableResult
    public func disconnect(connectorID: String) async throws -> Bool {
        try await connectors.disconnect(.with { $0.connectorID = connectorID }).revoked
    }

    // spendRequests returns the user's Link spend requests that wait for an approval in the Link app, as the api last
    // read them; polledAtMs is 0 before the first read and without an active Link grant.
    public func spendRequests() async throws -> Cocoonbox_Control_V1_SpendRequestsResponse {
        try await connectors.spendRequests(Cocoonbox_Control_V1_SpendRequestsRequest())
    }

    // answerConnector answers a connector approval (design §16.5): the broker
    // records the decision, then the agent's turn resumes and retries the call.
    // It returns false when the broker no longer holds the approval (expired, or
    // answered in the console); the turn resumes anyway, and the broker decides the retry.
    public func answerConnector(agentID: String, approvalID: String, decision: Cocoonbox_Control_V1_ApprovalDecision) async throws -> Bool {
        do {
            _ = try await connectors.approve(.with {
                $0.approvalID = approvalID
                $0.decision = decision
            })
        } catch let error as RPCError where error.code == .notFound {
            try await respond(agentID: agentID, approvalID: approvalID, approve: decision != .deny, text: "")
            return false
        }
        try await respond(agentID: agentID, approvalID: approvalID, approve: decision != .deny, text: "")
        return true
    }

    // MARK: Session

    public func hello(clientVersion: String) async throws -> Cocoonbox_Agent_V1_HelloResponse {
        try await session.hello(.with { $0.clientVersion = clientVersion })
    }

    // followEvents runs Session.Events until cancelled; see EventFollower.
    public func followEvents(
        onEvent: @escaping @Sendable (Cocoonbox_Agent_V1_Event) async -> Void,
        onResync: @escaping @Sendable () async -> Void,
        onDisconnect: @escaping @Sendable (any Error) async -> Void
    ) async {
        let session = session
        await EventFollower<Cocoonbox_Agent_V1_Event>.session().run(
            from: 0,
            subscribe: { offset, deliver in try await session.subscribeEvents(from: offset, deliver: deliver) },
            onEvent: onEvent,
            onResync: onResync,
            onDisconnect: onDisconnect
        )
    }

    public func activeOrigin() async throws -> String {
        try await session.activeOrigin(Cocoonbox_Agent_V1_ActiveOriginRequest()).origin
    }

    public func fillCredential(
        origin: String,
        username: String,
        secret: Cocoonbox_Agent_V1_FillCredentialRequest.OneOf_Secret,
        submit: Bool
    ) async throws -> Cocoonbox_Agent_V1_FillCredentialResponse {
        try await session.fillCredential(.with {
            $0.origin = origin
            $0.username = username
            $0.secret = secret
            $0.submit = submit
        })
    }

    // exec runs a command in the box as its desktop user: start names it, and input carries its
    // stdin, the end of stdin, signals and terminal sizes. stdout and stderr chunks reach output,
    // flagged true for stderr. It returns the exit code, or nil when the stream ended without one.
    public func exec(
        start: Cocoonbox_Agent_V1_ExecStart,
        input: some AsyncSequence<Cocoonbox_Agent_V1_ExecClientFrame, Never> & Sendable,
        output: @escaping @Sendable (Data, Bool) async throws -> Void
    ) async throws -> Int32? {
        try await session.exec(
            requestProducer: { writer in
                try await writer.write(.with { $0.start = start })
                for await frame in input {
                    try await writer.write(frame)
                }
            },
            onResponse: { response in
                var code: Int32?
                for try await frame in response.messages {
                    switch frame.body {
                    case .stdout(let data): try await output(data, false)
                    case .stderr(let data): try await output(data, true)
                    case .exitCode(let exit): code = exit
                    case nil: break
                    }
                }
                return code
            }
        )
    }

    var screenTransport: any ScreenTransport {
        GRPCScreenTransport(session: session)
    }

    // MARK: Agents

    public func listAgents() async throws -> [Cocoonbox_Agent_V1_Agent] {
        try await agents.list(Cocoonbox_Agent_V1_ListAgentsRequest()).agents
    }

    public func transcript(agentID: String, after seq: UInt64, limit: UInt32) async throws -> Cocoonbox_Agent_V1_TranscriptPage {
        try await agents.transcript(.with {
            $0.agentID = agentID
            $0.afterSeq = seq
            $0.limit = limit
        })
    }

    public func createAgent(name: String) async throws -> Cocoonbox_Agent_V1_Agent {
        try await agents.create(.with {
            $0.name = name
            $0.clientNonce = UUID().uuidString
        })
    }

    public func send(agentID: String, text: String) async throws {
        _ = try await agents.send(.with {
            $0.agentID = agentID
            $0.text = text
            $0.clientNonce = UUID().uuidString
        })
    }

    public func respond(agentID: String, approvalID: String, approve: Bool, text: String) async throws {
        _ = try await agents.respond(.with {
            $0.agentID = agentID
            $0.approvalID = approvalID
            $0.approve = approve
            $0.text = text
        })
    }

    public func cancel(agentID: String) async throws {
        _ = try await agents.cancel(.with { $0.agentID = agentID })
    }

    public func renameAgent(agentID: String, name: String) async throws {
        _ = try await agents.rename(.with {
            $0.agentID = agentID
            $0.name = name
        })
    }

    public func deleteAgent(agentID: String) async throws {
        _ = try await agents.delete(.with { $0.agentID = agentID })
    }

    public func endSession(agentID: String) async throws {
        _ = try await agents.end(.with { $0.agentID = agentID })
    }

    // describe renders a call error for the UI: the gRPC status code and message when there is one.
    public static func describe(_ error: any Error) -> String {
        if let rpc = error as? RPCError {
            return "\(rpc.code) \(rpc.message)"
        }
        return "\(error)"
    }

    // describeCredential renders an ActiveOrigin or FillCredential error; agents before v12 do not serve those calls.
    public static func describeCredential(_ error: any Error) -> String {
        guard let rpc = error as? RPCError else { return describe(error) }
        switch rpc.code {
        case .unimplemented:
            return "This box's desktop is too old for password sign-in. Upgrade the desktop first."
        case .failedPrecondition:
            let message = rpc.message.prefix(1).uppercased() + rpc.message.dropFirst()
            return message.last.map { ".!?".contains($0) } == true ? message : message + "."
        case .unavailable:
            return "The box's browser cannot be reached right now (\(rpc.message)). Try again in a moment."
        default:
            return describe(error)
        }
    }
}

// BoxGone is a held box that was deleted, with no box under its name to follow; name is the id when its name is unknown.
public struct BoxGone: Error, CustomStringConvertible {
    public let name: String

    public init(name: String) {
        self.name = name
    }

    public var description: String {
        if name.isEmpty { return "no box is selected" }
        return name.hasPrefix("cb-") ? "you have no box \(name)" : "the box \(name) was deleted"
    }
}

extension Cocoonbox_Control_V1_Box {
    // renewed maps each box of old that list lacks to the box of list with its name: a box deleted and created again.
    public static func renewed(from old: [Self], in list: [Self]) -> [String: Self] {
        var renewed: [String: Self] = [:]
        for box in old where !list.contains(where: { $0.id == box.id }) {
            if let again = list.first(where: { $0.name == box.name }) { renewed[box.id] = again }
        }
        return renewed
    }
}

public enum ConnectionError: Error, CustomStringConvertible {
    case plaintext(String)

    public var description: String {
        switch self {
        case .plaintext(let host): "\(host) is not on this Mac, so it needs https: the token would travel in plaintext"
        }
    }
}

final class AuthInterceptor: ClientInterceptor {
    let authorization: String
    let boxID = Mutex<String?>(nil)

    init(token: String) {
        authorization = "Bearer \(token)"
    }

    func intercept<Input: Sendable, Output: Sendable>(
        request: StreamingClientRequest<Input>,
        context: ClientContext,
        next: (StreamingClientRequest<Input>, ClientContext) async throws -> StreamingClientResponse<Output>
    ) async throws -> StreamingClientResponse<Output> {
        var request = request
        request.metadata.replaceOrAddString(authorization, forKey: "authorization")
        if context.descriptor.service.package == "cocoonbox.agent.v1", let box = boxID.withLock({ $0 }) {
            request.metadata.replaceOrAddString(box, forKey: "x-cocoonbox-box")
        }
        return try await next(request, context)
    }
}
