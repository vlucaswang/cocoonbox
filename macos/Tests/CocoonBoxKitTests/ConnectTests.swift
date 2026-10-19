import CocoonBoxBridge
import CocoonBoxKit
import Foundation
import Synchronization
import Testing

// ConnectTests runs the bridge and the app's approval call against a MockBox of
// its own, whose mock-only `mock/connect` sets the Connect door's grants and
// failures (design §16.3, §16.5). The box stays hibernated in the bridge tests.
@Suite struct ConnectTests {
    nonisolated(unsafe) static let hub: [String: Any] = ["id": "mockhub", "account": "octo", "tools": [["name": "list_issues", "read_only": true], ["name": "create_issue"]]]
    static let plain = ToolCatalog.control.compactMap { $0["name"] as? String } + ToolCatalog.boxFallback.compactMap { $0["name"] as? String }

    @Test func mergesConnectorToolsAndRoutesTheirCalls() async throws {
        let mock = try await MockBoxProcess()
        defer { mock.stop() }
        try await mock.connect(["grants": [Self.hub, ["id": "browser", "tools": [["name": "click"]]]]])
        let client = try await BridgeClient(mock)
        defer { client.close() }

        let tools = try await client.tools()
        let names = tools.compactMap { $0["name"] as? String }
        #expect(names == Self.plain + ["mockhub_list_issues", "mockhub_create_issue"])
        let create = try #require(tools.first { $0["name"] as? String == "mockhub_create_issue" })
        #expect((create["inputSchema"] as? [String: Any])?["properties"] == nil)
        #expect((create["annotations"] as? [String: Any])?["destructiveHint"] as? Bool == true)

        let result = try await client.tool("mockhub_create_issue", ["title": "x"])
        #expect(Self.text(result) == #"mockhub_create_issue ran with {"title":"x"}"#)
        let status = try await client.tool("box_status")
        #expect((status["structuredContent"] as? [String: Any])?["state"] as? String == "hibernated")
        let log = mock.log()
        #expect(log.contains("connect rpc=initialize client=bridge-test"))
        #expect(log.contains("connect rpc=tools/call tool=mockhub_create_issue"))
        #expect(!log.contains("tool=box_ensure"))
    }

    @Test func passesListChangedOnWhenTheToolsChange() async throws {
        let mock = try await MockBoxProcess()
        defer { mock.stop() }
        try await mock.connect(["grants": [Self.hub]])
        let client = try await BridgeClient(mock)
        defer { client.close() }
        #expect(try await client.tools().count == Self.plain.count + 2)
        #expect(await until { mock.log().contains("connect stream") })

        try await mock.connect(["grants": [] as [Any]])
        #expect(await until { client.listChanged == 1 })
        #expect(try await client.tools().compactMap { $0["name"] as? String } == Self.plain)

        try await mock.connect(["grants": [] as [Any]])
        try await Task.sleep(for: .seconds(1))
        #expect(client.listChanged == 1)
    }

    @Test func aHibernatedCbdeskBoxNeverListsAnotherBoxsCachedTools() async throws {
        let mock = try await MockBoxProcess()
        defer { mock.stop() }
        let cbdesk = try #require(try await mock.connection.firstBox()).id
        try await Self.core(mock)
        let core = try await BridgeClient(mock, box: "core1")
        _ = try await core.tool("shell", ["command": "true"])
        let live = try await core.tools().first { $0["name"] as? String == "script_run" }
        #expect((live?["_meta"] as? [String: Any])?["anthropic/alwaysLoad"] as? Bool == true, "the bridge forwards script_run's _meta")
        core.close()

        let desk = try await BridgeClient(mock, box: cbdesk)
        defer { desk.close() }
        let names = try await desk.tools().compactMap { $0["name"] as? String }
        let boxTools = names.filter { ToolCatalog.boxNames.contains($0) || $0 == "script_run" }
        #expect(boxTools.count == ToolCatalog.boxFallback.count && !names.contains("script_run"), "\(names)")
    }

    @Test func aCbcoreDefaultBoxAddsItsOwnInstructionsAndACbdeskBoxAddsNone() async throws {
        let mock = try await MockBoxProcess()
        defer { mock.stop() }
        let cbdesk = try #require(try await mock.connection.firstBox()).id
        let desk = try await BridgeClient(mock, box: cbdesk)
        #expect(desk.instructions == Bridge.instructions, "a cbdesk box's bridge instructions stay the bridge's own")
        desk.close()
        try await Self.core(mock)

        let running = try await BridgeClient(mock, box: "core1")
        #expect(running.instructions == Bridge.instructions + " The default box's own instructions follow. Mock cbcore box: write one script with script_run for a multi-step task.")
        _ = try await running.tool("box_hibernate", ["box": "core1"])
        running.close()
        let hibernated = try await BridgeClient(mock, box: "core1")
        #expect(hibernated.instructions == running.instructions, "a hibernated cbcore box keeps its cached instructions")
        let cached = try await hibernated.tools().first { $0["name"] as? String == "script_run" }
        #expect((cached?["_meta"] as? [String: Any])?["anthropic/alwaysLoad"] as? Bool == true, "the cached list keeps script_run's _meta")
        hibernated.close()

        let again = try await BridgeClient(mock, box: cbdesk)
        defer { again.close() }
        #expect(again.instructions == Bridge.instructions)
    }

    @Test func anAndroidBoxListsTheFallbackUntilItAnswersThenItsOwnToolsAndInstructions() async throws {
        let mock = try await MockBoxProcess()
        defer { mock.stop() }
        let phone = try await mock.connection.createBox(name: "phone", image: "cbdroid").box.id
        #expect(await until(seconds: 10) { (try? await mock.connection.listBoxes())?.first { $0.id == phone }?.state == .running })
        _ = try await mock.connection.hibernate(boxID: phone)
        #expect(await until(seconds: 10) { (try? await mock.connection.listBoxes())?.first { $0.id == phone }?.state == .hibernated })

        let fresh = try await BridgeClient(mock, box: "phone")
        #expect(fresh.instructions == Bridge.instructions, "a phone that never answered adds no instructions")
        let names = { (client: BridgeClient) in try await client.tools().compactMap { $0["name"] as? String } }
        #expect(try await names(fresh) == Self.plain)
        #expect(fresh.listChanged == 0)
        _ = try await fresh.tool("shell", ["command": "true"])
        #expect(await until { fresh.listChanged == 1 }, "the bridge sends tools/list_changed once the woken phone answers")
        let live = try await names(fresh)
        #expect(live.contains("gui_plan") && live.contains("script_run") && !live.contains { $0.hasPrefix("browser_") }, "\(live)")
        fresh.close()

        let again = try await BridgeClient(mock, box: "phone")
        defer { again.close() }
        #expect(again.instructions == Bridge.instructions + " The default box's own instructions follow. These tools act on an Android 16 phone (720x1280) that runs in a cloud computer: Ubuntu 24.04 with no desktop. shell has adb.")
    }

    @Test func aBoxMovedToAnotherImageGetsItsNewInstructions() async throws {
        let mock = try await MockBoxProcess()
        defer { mock.stop() }
        let cbdesk = try #require(try await mock.connection.firstBox()).id
        let desk = try await BridgeClient(mock, box: cbdesk)
        _ = try await desk.tool("shell", ["command": "true"])
        desk.close()
        #expect(desk.instructions == Bridge.instructions)

        _ = try await mock.connection.rebase(boxID: cbdesk, image: "cbcore:v1")
        #expect(await until(seconds: 10) { (try? await mock.connection.firstBox())?.image == "cbcore:v1" })
        let core = try await BridgeClient(mock, box: cbdesk)
        defer { core.close() }
        #expect(core.instructions == Bridge.instructions + " The default box's own instructions follow. Mock cbcore box: write one script with script_run for a multi-step task.")
    }

    @Test func aBoxWithGuiPlanButNoScriptRunGetsItsOwnInstructions() async throws {
        let mock = try await MockBoxProcess()
        defer { mock.stop() }
        let box = try #require(try await mock.connection.firstBox()).id
        _ = try await mock.connection.rebase(boxID: box, image: "plans:v1")
        #expect(await until(seconds: 10) { (try? await mock.connection.firstBox())?.image == "plans:v1" })
        let client = try await BridgeClient(mock, box: box)
        defer { client.close() }
        #expect(try await !client.tools().contains { $0["name"] as? String == "script_run" })
        #expect(client.instructions == Bridge.instructions + " The default box's own instructions follow. Mock box that runs GUI plans.")
    }

    @Test func theBridgeNamesTheUsersKeysWithinTheInstructionsCap() async throws {
        let mock = try await MockBoxProcess()
        defer { mock.stop() }
        _ = try await mock.connection.addKey(.with {
            $0.name = "ATTIO_API_KEY"
            $0.hosts = ["api.attio.com"]
            $0.value = "synthetic-0123456789"
        })
        let client = try await BridgeClient(mock)
        defer { client.close() }
        #expect(client.instructions == Bridge.instructions + " " + Bridge.keysHead + " ATTIO_API_KEY for api.attio.com as Authorization: Bearer $ATTIO_API_KEY.")

        let basic = Cocoonbox_Control_V1_Key.with {
            $0.name = "STRIPE"
            $0.hosts = ["api.stripe.com"]
            $0.header = "Authorization"
            $0.template = "Basic {}:"
        }
        let query = Cocoonbox_Control_V1_Key.with {
            $0.name = "SERP"
            $0.hosts = ["serpapi.com"]
            $0.query = "api_key"
        }
        #expect(Bridge.sentAs(basic) == "Authorization: Basic base64($STRIPE:)")
        #expect(Bridge.sentAs(query) == "query ?api_key=$SERP")
        let many = (0..<32).map { i in
            Cocoonbox_Control_V1_Key.with {
                $0.name = "KEY_\(i)"
                $0.hosts = ["api\(i).example.com"]
                $0.header = "Authorization"
                $0.template = "Bearer {}"
            }
        }
        let long = String(repeating: "x", count: 1600)
        let capped = Bridge.withKeys(long, many)
        #expect(capped.count <= Bridge.maxInstructions && capped.contains("KEY_0 for") && capped.contains("more in `cocoonbox keys ls`"), "\(capped.count)")
        #expect(Bridge.withKeys(long, []) == long)
        #expect(Bridge.withKeys(String(repeating: "x", count: 2040), many) == String(repeating: "x", count: 2040))
    }

    @Test func aCbcoreBoxNamesTheKeysAndLinkInShellsDescription() async throws {
        let mock = try await MockBoxProcess()
        defer { mock.stop() }
        _ = try await mock.connection.addKey(.with {
            $0.name = "ATTIO_API_KEY"
            $0.hosts = ["api.attio.com"]
            $0.value = "synthetic-0123456789"
        })
        try await mock.connect(["grants": [["id": "link"]]])
        let desk = try await BridgeClient(mock)
        let shell = { (client: BridgeClient) in try await client.tools().first { $0["name"] as? String == "shell" }?["description"] as? String }
        #expect(try await shell(desk)?.contains(Bridge.keysHead) == false)
        desk.close()
        try await Self.core(mock)

        let core = try await BridgeClient(mock, box: "core1")
        defer { core.close() }
        #expect(core.instructions == Bridge.instructions + " The default box's own instructions follow. Mock cbcore box: write one script with script_run for a multi-step task.")
        #expect(try await shell(core) == "Run a shell command on the box. " + Bridge.linkLine + " " + Bridge.keysHead + " ATTIO_API_KEY for api.attio.com as Authorization: Bearer $ATTIO_API_KEY.")
    }

    @Test func theBridgeAddsTheLinkLineOnlyWithAnActiveLinkGrant() async throws {
        let mock = try await MockBoxProcess()
        defer { mock.stop() }
        try await mock.connect(["grants": [["id": "link", "status": "needs_reconnect"]]])
        let lost = try await BridgeClient(mock)
        #expect(lost.instructions == Bridge.instructions)
        lost.close()
        try await mock.connect(["grants": [["id": "link"]]])
        let client = try await BridgeClient(mock)
        defer { client.close() }
        #expect(client.instructions == Bridge.instructions + " " + Bridge.linkLine)
        #expect(Bridge.withKeys(String(repeating: "x", count: 2000), [], link: true) == String(repeating: "x", count: 2000))
    }

    @Test func codexGetsOneLineOnACbcoreBoxAndTheBridgeTextOnACbdeskBox() async throws {
        let mock = try await MockBoxProcess()
        defer { mock.stop() }
        let cbdesk = try #require(try await mock.connection.firstBox()).id
        let desk = try await BridgeClient(mock, box: cbdesk, client: "codex-mcp-client")
        #expect(desk.instructions == Bridge.instructions)
        desk.close()
        try await Self.core(mock)
        let core = try await BridgeClient(mock, box: "core1", client: "codex-mcp-client")
        defer { core.close() }
        #expect(core.instructions == Bridge.codexInstructions("Mock cbcore box: write one script with script_run for a multi-step task."))
        #expect(core.instructions == "Mock cbcore box: write one script with script_run for a multi-step task. It is the user's cocoonbox, not this Mac: GUI work in an app goes to gui_plan; commands go to shell.")
        #expect(Bridge.codexInstructions("These tools act on an Android 16 phone (720x1280) that runs in a cloud computer: Ubuntu 24.04 with no desktop. shell is one session.") == "These tools act on an Android 16 phone (720x1280) that runs in a cloud computer: Ubuntu 24.04 with no desktop. It is the user's cocoonbox, not this Mac: GUI work in an app goes to gui_plan; commands go to shell.")
    }

    @Test func anAppSwitchToABoxWithOtherToolsSendsListChanged() async throws {
        let mock = try await MockBoxProcess()
        defer { mock.stop() }
        let cbdesk = try #require(try await mock.connection.firstBox()).id
        try await Self.core(mock)
        let creator = try await BridgeClient(mock)
        _ = try await creator.tool("shell", ["box": "core1", "command": "true"])
        creator.close()
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("link-\(UUID().uuidString.prefix(8))")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let path = directory.appendingPathComponent("a.sock").path
        let server = try AgentLinkServer(path: path, queue: DispatchQueue(label: "test"), defaultBox: { _ in nil }) { _, _, _ in }
        defer { server.close() }

        let client = try await BridgeClient(mock, box: cbdesk, link: try #require(AgentLinkClient(path: path, agent: "agent-1")))
        defer { client.close() }
        #expect(try await !client.tools().contains { $0["name"] as? String == "script_run" })
        server.push(agent: "agent-1", box: "core1")
        try await Task.sleep(for: .milliseconds(300))
        _ = try await client.tool("box_status")
        #expect(client.listChanged == 1)
        #expect(try await client.tools().contains { $0["name"] as? String == "script_run" })
    }

    @Test func emptyMergeWithNoGrants() async throws {
        let mock = try await MockBoxProcess()
        defer { mock.stop() }
        let client = try await BridgeClient(mock)
        defer { client.close() }
        #expect(try await client.tools().compactMap { $0["name"] as? String } == Self.plain)
    }

    @Test func theKitReadsTheCatalogListsToolsAndDisconnects() async throws {
        let mock = try await MockBoxProcess()
        defer { mock.stop() }
        try await mock.connect(["grants": [Self.hub]])
        let connection = mock.connection
        #expect(try await connection.catalog().map(\.id) == ["github", "custom"])
        #expect(try await connection.listConnectors().map(\.tools.count) == [0])
        #expect(try await connection.listConnectors(withTools: true).first?.tools.map(\.name) == ["list_issues", "create_issue"])
        #expect(try await connection.spendRequests().polledAtMs == 0)
        #expect(try await connection.disconnect(connectorID: "mockhub"))
        #expect(try await connection.listConnectors().isEmpty)
        await #expect(throws: (any Error).self) { try await connection.disconnect(connectorID: "mockhub") }
    }

    @Test func keepsWorkingWhileTheDoorFailsAndRecovers() async throws {
        let mock = try await MockBoxProcess()
        defer { mock.stop() }
        try await mock.connect(["grants": [Self.hub], "fail": 503])
        let client = try await BridgeClient(mock)
        defer { client.close() }
        let started = ContinuousClock.now
        #expect(try await client.tools().compactMap { $0["name"] as? String } == Self.plain)
        #expect(ContinuousClock.now - started < .seconds(3) + .seconds(2), "a failing door holds the first list at most connectWait (3 s)")
        let status = try await client.tool("box_status")
        #expect((status["structuredContent"] as? [String: Any])?["state"] as? String == "hibernated")

        try await mock.connect(["grants": [Self.hub]])
        #expect(await until(seconds: 10) { client.listChanged == 1 })
        #expect(try await client.tools().count == Self.plain.count + 2)
    }

    @Test func aHungDoorDelaysOnlyTheFirstList() async throws {
        let mock = try await MockBoxProcess()
        defer { mock.stop() }
        try await mock.connect(["grants": [Self.hub], "hang": true])
        let client = try await BridgeClient(mock)
        defer { client.close() }
        let started = ContinuousClock.now
        #expect(try await client.tools().compactMap { $0["name"] as? String } == Self.plain)
        let waited = ContinuousClock.now - started
        #expect(waited > .seconds(2) && waited < .seconds(10))
        let answers = try await client.answers(["tools/list", "ping"])
        #expect(((answers[0].answer["result"] as? [String: Any])?["tools"] as? [Any])?.count == Self.plain.count)
        #expect(answers[0].at - answers[1].at < .seconds(2), "the second list waits on the door")
        let status = try await client.tool("box_status")
        #expect((status["structuredContent"] as? [String: Any])?["state"] as? String == "hibernated")
    }

    @Test func anApiWithoutTheDoorEndsTheWatch() async throws {
        let mock = try await MockBoxProcess()
        defer { mock.stop() }
        try await mock.connect(["fail": 404])
        let client = try await BridgeClient(mock)
        defer { client.close() }
        #expect(try await client.tools().compactMap { $0["name"] as? String } == Self.plain)
        try await mock.connect(["grants": [Self.hub]])
        try await Task.sleep(for: .seconds(3))
        #expect(client.listChanged == 0)
        #expect(mock.log().components(separatedBy: "connect rpc=initialize").count == 1)
    }

    @Test func approvalAnswersTheBrokerThenResumesTheAgent() async throws {
        let mock = try await MockBoxProcess()
        defer { mock.stop() }
        let connection = mock.connection
        let box = try #require(try await connection.firstBox())
        _ = try await connection.ensure(boxID: box.id)
        #expect(await until { (try? await connection.firstBox())?.state == .running })
        connection.boxID = box.id
        let agent = try await connection.createAgent(name: "connect")

        for (prompt, decision, outcome) in [("open an issue", .once, "Created issue #42 (once)."), ("another issue", .always, "Created issue #42 (always)."), ("one more issue", .deny, "Denied.")] as [(String, Cocoonbox_Control_V1_ApprovalDecision, String)] {
            try await connection.send(agentID: agent.id, text: prompt)
            var approval: Cocoonbox_Agent_V1_ApprovalRequest?
            #expect(await until {
                approval = await Self.entries(connection, agent.id).compactMap { entry -> Cocoonbox_Agent_V1_ApprovalRequest? in
                    if case .approval(let request) = entry.body, !mock.log().contains(request.approvalID) { return request }
                    return nil
                }.last
                return approval != nil
            })
            let request = try #require(approval)
            #expect(request.kind == "connector" && request.approvalID.hasPrefix("apr_") && request.summary.contains("create_issue"))
            #expect(try await connection.answerConnector(agentID: agent.id, approvalID: request.approvalID, decision: decision))
            #expect(await until { await Self.entries(connection, agent.id).contains { Self.says($0, outcome) } }, "\(prompt)")
            #expect(mock.log().contains("connectors approve \(request.approvalID) \(decision)"))
        }
    }

    @Test func anApprovalAnsweredInTheConsoleTellsTheUser() async throws {
        let mock = try await MockBoxProcess()
        defer { mock.stop() }
        let connection = mock.connection
        let box = try #require(try await connection.firstBox())
        _ = try await connection.ensure(boxID: box.id)
        #expect(await until { (try? await connection.firstBox())?.state == .running })
        connection.boxID = box.id
        let agent = try await connection.createAgent(name: "console")
        try await connection.send(agentID: agent.id, text: "open an issue")
        var approval: Cocoonbox_Agent_V1_ApprovalRequest?
        #expect(await until {
            approval = await Self.entries(connection, agent.id).compactMap { entry -> Cocoonbox_Agent_V1_ApprovalRequest? in
                if case .approval(let request) = entry.body { return request }
                return nil
            }.first
            return approval != nil
        })
        let request = try #require(approval)
        try await mock.rpc("mock/answer", ["approval_id": request.approvalID])

        #expect(try await !connection.answerConnector(agentID: agent.id, approvalID: request.approvalID, decision: .once))
        #expect(await until { await Self.entries(connection, agent.id).contains { Self.says($0, "Refused: denied.") } })
    }

    // core creates a running cbcore box, the family whose box tools add script_run.
    static func core(_ mock: MockBoxProcess) async throws {
        let id = try await mock.connection.createBox(name: "core1", image: "cbcore").box.id
        #expect(await until(seconds: 10) { (try? await mock.connection.listBoxes())?.first { $0.id == id }?.state == .running })
    }

    static func text(_ result: [String: Any]) -> String {
        (result["content"] as? [[String: Any]] ?? []).compactMap { $0["text"] as? String }.joined(separator: " ")
    }

    static func entries(_ connection: BoxConnection, _ agent: String) async -> [Cocoonbox_Agent_V1_TranscriptEntry] {
        (try? await connection.transcript(agentID: agent, after: 0, limit: 500).entries) ?? []
    }

    static func says(_ entry: Cocoonbox_Agent_V1_TranscriptEntry, _ text: String) -> Bool {
        switch entry.body {
        case .toolResult(let result): result.text == text
        case .text(let block): block.text == text
        default: false
        }
    }
}

private func until(seconds: Double = 5, _ condition: () async -> Bool) async -> Bool {
    let deadline = Date().addingTimeInterval(seconds)
    while await !condition(), Date() < deadline {
        try? await Task.sleep(for: .milliseconds(20))
    }
    return await condition()
}

// MockBoxProcess runs MockBox with its log in a temporary directory, on a random
// port; it tries another port when MockBox cannot listen on that one.
final class MockBoxProcess: @unchecked Sendable {
    let endpoint: Endpoint
    let connection: BoxConnection
    let root: URL
    private let input: Pipe

    init() async throws {
        root = try Harness.temporary("connect")
        (endpoint, input) = try await Harness.mockBox(["--boot-ms", "100"], log: root.appendingPathComponent("mockbox.log"), environment: Harness.environment.merging(["HOME": root.path]) { $1 })
        connection = try BoxConnection(endpoint: endpoint, token: "mock-token")
    }

    func stop() {
        connection.close()
        try? input.fileHandleForWriting.close()
    }

    func log() -> String {
        (try? String(contentsOf: root.appendingPathComponent("mockbox.log"), encoding: .utf8)) ?? ""
    }

    // connect sets the door through the mock-only method: grants, fail (an HTTP status) and hang.
    func connect(_ params: [String: Any]) async throws {
        try await rpc("mock/connect", params)
    }

    func rpc(_ method: String, _ params: [String: Any]) async throws {
        var request = URLRequest(url: URL(string: endpoint.url + "/cocoonbox.mcp/connect")!)
        request.httpMethod = "POST"
        request.setValue("Bearer mock-token", forHTTPHeaderField: "Authorization")
        request.httpBody = try JSONSerialization.data(withJSONObject: ["jsonrpc": "2.0", "id": 1, "method": method, "params": params])
        let (body, response) = try await URLSession.shared.data(for: request)
        #expect((response as? HTTPURLResponse)?.statusCode == 200)
        #expect(String(decoding: body, as: UTF8.self).contains(#""result""#))
    }
}

// BridgeClient plays a local agent: it feeds the bridge newline-delimited
// JSON-RPC and reads its answers and notifications.
final class BridgeClient: Sendable {
    private let initialized = Mutex("")
    private let input = Pipe()
    private let output = Lines()
    private let next = Mutex(1)
    private let task: Task<Void, Never>

    init(_ mock: MockBoxProcess, box: String? = nil, link: AgentLinkClient? = nil, client: String = "bridge-test") async throws {
        let output = output
        let bridge = Bridge(endpoint: mock.endpoint, token: "mock-token", wake: true, box: box, link: link, cache: mock.root.appendingPathComponent("tools.json")) { data in
            output.state.withLock { $0 += data.split(separator: UInt8(ascii: "\n")).map { Data($0) } }
        }
        let reader = input.fileHandleForReading
        task = Task { await bridge.run(input: reader) }
        let answer = try await call("initialize", ["protocolVersion": "2025-06-18", "capabilities": [:] as [String: Any], "clientInfo": ["name": client, "version": "1"]])
        let text = (answer["result"] as? [String: Any])?["instructions"] as? String ?? ""
        initialized.withLock { $0 = text }
        send(["jsonrpc": "2.0", "method": "notifications/initialized"])
    }

    var instructions: String {
        initialized.withLock { $0 }
    }

    var listChanged: Int {
        messages().filter { $0["method"] as? String == "notifications/tools/list_changed" }.count
    }

    func tools() async throws -> [[String: Any]] {
        try await call("tools/list")["result"].flatMap { ($0 as? [String: Any])?["tools"] as? [[String: Any]] } ?? []
    }

    func tool(_ name: String, _ arguments: [String: Any] = [:]) async throws -> [String: Any] {
        try await call("tools/call", ["name": name, "arguments": arguments])["result"] as? [String: Any] ?? [:]
    }

    func call(_ method: String, _ params: [String: Any] = [:]) async throws -> [String: Any] {
        let id = next.withLock { id in defer { id += 1 }; return id }
        send(["jsonrpc": "2.0", "id": id, "method": method, "params": params])
        var answer: [String: Any]?
        _ = await until(seconds: 15) {
            answer = messages().first { $0["id"] as? Int == id }
            return answer != nil
        }
        return try #require(answer, "no answer to \(method)")
    }

    func answers(_ methods: [String]) async throws -> [(answer: [String: Any], at: ContinuousClock.Instant)] {
        let ids = methods.map { _ in next.withLock { id in defer { id += 1 }; return id } }
        let batch = try zip(ids, methods).map { try JSONSerialization.data(withJSONObject: ["jsonrpc": "2.0", "id": $0, "method": $1, "params": [:] as [String: Any]]) + Data("\n".utf8) }
        input.fileHandleForWriting.write(Data(batch.joined()))
        var arrived: [Int: (answer: [String: Any], at: ContinuousClock.Instant)] = [:]
        _ = await until(seconds: 15) {
            for message in messages() {
                if let id = message["id"] as? Int, ids.contains(id), arrived[id] == nil { arrived[id] = (message, .now) }
            }
            return arrived.count == ids.count
        }
        return try ids.map { try #require(arrived[$0], "no answer to \(methods)") }
    }

    func close() {
        try? input.fileHandleForWriting.close()
    }

    private final class Lines: Sendable {
        let state = Mutex<[Data]>([])
    }

    private func send(_ message: [String: Any]) {
        guard let data = try? JSONSerialization.data(withJSONObject: message) else { return }
        input.fileHandleForWriting.write(data + Data("\n".utf8))
    }

    private func messages() -> [[String: Any]] {
        output.state.withLock { $0 }.compactMap { try? JSONSerialization.jsonObject(with: $0) as? [String: Any] }
    }
}
