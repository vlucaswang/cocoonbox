import CocoonBoxKit
import Foundation

// Bridge is the stdio MCP server a local agent runs as `cocoonbox mcp`
// (design.md §13.2, §19.2). Lifecycle tools go to the control MCP, box tools
// to the MCP of the box their `box` argument names, else the default box:
// `--box`, else the newest running box when the bridge first needs one. It
// opens one session per box on first use and wakes a box on its first
// box-tool call unless `wake` is off. With the app's link it reports each
// box-tool call to the app and takes the default box the app sends. Connector
// tools come from the Connect door (§16.3) and go back to it, with no box. The
// token stays in memory and in the Authorization header of its requests.
public actor Bridge {
    static let defaulted: Set<String> = ["box_status", "box_ensure", "box_hibernate", "box_save", "box_upgrade"]
    static let connectWait = Duration.seconds(3)
    public static let instructions = "These tools run on the user's cocoonbox boxes, not on this Mac. box_* tools manage the boxes; the box tools act on one box, the one their box argument names, else the default box of this session. Tools named <connector>_<tool> act on the user's connected accounts, such as GitHub, through CocoonBox Connect, not on a box."
    static let codexClient = "codex-mcp-client"
    // maxInstructions is Claude Code's cap on a server's instructions and on a tool's description.
    public static let maxInstructions = 2048
    static let keysWait = Duration.milliseconds(1500)
    public static let keysHead = "Box shells hold the user's API keys as env vars whose values are placeholders; the box's network swaps in the key only on the listed hosts, so send them as shown and never ask for a key:"
    public static let linkLine = "The user's Link wallet is connected: you may pay from a box, but only for a purchase the user asked for and only with a one-time card from a Link spend request (link-cli in a box shell, with a truthful --context) that the user approved. Read the card only with link-cli's --output-file under /tmp, never cat or print that file, and fill each field of that checkout with computer_act type_from_file. Never enter the user's own card or other payment details."

    private let endpoint: Endpoint
    private let token: String
    private let wake: Bool
    private let link: AgentLinkClient?
    private let cache: URL
    private let output: @Sendable (Data) -> Void
    private var client: [String: Any] = [:]
    private var control: MCPUpstream?
    private var sessions: [String: MCPUpstream] = [:]
    private var opening: [String: Task<MCPUpstream, any Error>] = [:]
    // ids maps every box name and id of the user to the box id.
    private var ids: [String: String] = [:]
    // names keeps the name of every box id ever listed, so a deleted box's name stays known.
    private var names: [String: String] = [:]
    // entries holds each box's entry of the last box_list.
    private var entries: [String: [String: Any]] = [:]
    private var pinned: String?
    private var defaultBox: String?
    // boxTools are the box tools the client last got: those of the default box.
    private var boxTools: [[String: Any]]
    // boxLists holds the own tools/list of each box opened in this session.
    private var boxLists: [String: [[String: Any]]] = [:]
    private var boxInstructions: [String: String] = [:]
    private var shellKeys: (keys: [Cocoonbox_Control_V1_Key], link: Bool)?
    private var listedBox = false
    private var connect: MCPUpstream?
    private var connectWatch: Task<Void, Never>?
    private var connectTools: [[String: Any]] = []
    // listedConnect is the connector list the client last got, nil before its first tools/list.
    private var listedConnect: [[String: Any]]?
    // connectFirst ends at the first connector list, which cancels it, or connectWait after the watch starts.
    private var connectFirst: Task<Void, Never>?
    private var nextID = 1

    public init(endpoint: Endpoint, token: String, wake: Bool, box: String? = nil, link: AgentLinkClient? = nil, cache: URL, output: @escaping @Sendable (Data) -> Void) {
        self.endpoint = endpoint
        self.token = token
        self.wake = wake
        pinned = box
        self.link = link
        self.cache = cache
        self.output = output
        boxTools = ToolCatalog.boxFallback
    }

    // run serves newline-delimited JSON-RPC from stdin until it closes, then ends the upstream sessions.
    public func run(input: FileHandle = .standardInput) async {
        do {
            for try await line in input.bytes.lines where !line.isEmpty {
                guard let message = try? JSONSerialization.jsonObject(with: Data(line.utf8)) as? [String: Any] else { continue }
                Task { await self.handle(message) }
            }
        } catch {}
        await shutdown()
    }

    public func shutdown() async {
        connectWatch?.cancel()
        for upstream in sessions.values {
            await upstream.close()
        }
        await control?.close()
        await connect?.close()
        sessions = [:]
        control = nil
        connect = nil
    }

    func handle(_ message: [String: Any]) async {
        guard let method = message["method"] as? String else { return }
        let params = message["params"] as? [String: Any] ?? [:]
        guard let id = message["id"] else {
            if method == "notifications/initialized" { await warmUp() }
            return
        }
        switch method {
        case "initialize":
            client = params
            startConnect()
            reply(id, [
                "protocolVersion": params["protocolVersion"] as? String ?? "2025-06-18",
                "capabilities": ["tools": ["listChanged": true], "resources": [:] as [String: Any]],
                "serverInfo": ["name": "cocoonbox", "version": "0.16.0"],
                "instructions": await instructionsText(),
            ])
        case "ping":
            reply(id, [:])
        case "tools/list":
            await connectReady()
            listedConnect = connectTools
            listedBox = true
            boxTools = await defaultTools()
            reply(id, ["tools": ToolCatalog.control + boxTools.map { withShellKeys(ToolCatalog.withBox($0)) } + connectTools])
        case "resources/list":
            await listResources(id)
        case "resources/read":
            await readResource(id, params)
        case "tools/call":
            let name = params["name"] as? String ?? ""
            if ToolCatalog.controlNames.contains(name) {
                await callControl(id, params)
            } else if await isConnector(name) {
                await callConnect(id, params)
            } else {
                await callBox(id, params)
            }
        default:
            send(["jsonrpc": "2.0", "id": id, "error": ["code": -32601, "message": "method not found: \(method)"]])
        }
    }

    // warmUp opens the default box's session right away when that box runs, so the tool list is the box's own.
    private func warmUp() async {
        guard let box = try? await target(nil), await entry(box)?["state"] as? String == "running" else { return }
        _ = try? await readyBox(box, progress: nil, running: true)
    }

    // callControl forwards a lifecycle tool, naming the default box when the call names none.
    private func callControl(_ id: Any, _ params: [String: Any]) async {
        var params = params
        let name = params["name"] as? String ?? ""
        var arguments = params["arguments"] as? [String: Any] ?? [:]
        if Self.defaulted.contains(name), arguments["box"] == nil {
            do {
                arguments["box"] = try await target(nil)
                params["arguments"] = arguments
            } catch let failure as ToolFailure where pinned != nil {
                return reply(id, failure.result)
            } catch {}
        }
        var deleted: String?
        if name == "box_delete", let ref = arguments["box"] as? String {
            deleted = try? await resolve(ref)
        }
        await forward(id, params, to: { try await self.openControl() })
        if name == "box_create" || name == "box_delete" { ids = [:] }
        if let deleted {
            await sessions.removeValue(forKey: deleted)?.close()
            if defaultBox == deleted { defaultBox = nil }
        }
    }

    private func callBox(_ id: Any, _ params: [String: Any]) async {
        var params = params
        var arguments = params["arguments"] as? [String: Any] ?? [:]
        let ref = arguments.removeValue(forKey: "box") as? String
        params["arguments"] = arguments
        let progress = (params["_meta"] as? [String: Any])?["progressToken"]
        let box: String
        do {
            box = try await target(ref)
        } catch let failure as ToolFailure {
            return reply(id, failure.result)
        } catch {
            return reply(id, ToolFailure("Could not reach cocoonbox: \(error)").result)
        }
        do {
            relay(id, try await callOn(box, params, progress))
        } catch let refusal where Self.refuses(refusal) {
            do {
                let next = try await follow(box, explicit: ref?.hasPrefix("cb-") == true)
                guard next != box else { return reply(id, Self.result(of: refusal, box: box)) }
                relay(id, try await callOn(next, params, progress))
            } catch {
                reply(id, Self.result(of: error, box: box))
            }
        } catch {
            reply(id, Self.result(of: error, box: box))
        }
    }

    // callOn sends one box-tool call to box, on a new session once when the old one expired or the edge lost the agent.
    private func callOn(_ box: String, _ params: [String: Any], _ progress: Any?) async throws -> [String: Any] {
        link?.report(box: box)
        var retried = false
        while true {
            do {
                return try await call(try await readyBox(box, progress: progress), "tools/call", params)
            } catch MCPUpstream.Failure.sessionExpired where !retried {
                sessions[box] = nil
            } catch MCPUpstream.Failure.http(let status) where !retried && (status == 502 || status == 503) {
                sessions[box] = nil
            }
            retried = true
        }
    }

    // follow checks box against a fresh list after the edge refused it: it keeps a box the user has or one it never listed,
    // moves a deleted default box to the box created again under its name, and refuses an id the call named, giving the new one.
    private func follow(_ box: String, explicit: Bool) async throws -> String {
        let list = try await listBoxes()
        guard let name = names[box], !list.contains(where: { $0["box_id"] as? String == box }) else { return box }
        sessions[box] = nil
        boxLists[box] = nil
        boxInstructions[box] = nil
        try? FileManager.default.removeItem(at: cacheFile(box))
        try? FileManager.default.removeItem(at: cacheFile(box, "instructions.txt"))
        try? FileManager.default.removeItem(at: cacheFile(box, "image"))
        guard let next = ids[name] else { throw BoxGone(name: name) }
        if explicit { throw ToolFailure("The box \(box) was deleted; the box named \(name) is now \(next).") }
        if defaultBox == box { defaultBox = next }
        return next
    }

    // listResources lists the resources of the default box when its session is open, else none; it never wakes a box.
    private func listResources(_ id: Any) async {
        guard let box = try? await target(nil), let upstream = sessions[box],
              let result = (try? await call(upstream, "resources/list", [:]))?["result"] as? [String: Any] else {
            return reply(id, ["resources": [] as [Any]])
        }
        reply(id, result)
    }

    // readResource reads a resource of the default box, waking it as a box tool does.
    private func readResource(_ id: Any, _ params: [String: Any]) async {
        do {
            let upstream = try await readyBox(try await target(nil), progress: nil)
            relay(id, try await call(upstream, "resources/read", params))
        } catch let failure as ToolFailure {
            let text = (failure.result["content"] as? [[String: Any]])?.first?["text"] as? String ?? "\(failure.result)"
            send(["jsonrpc": "2.0", "id": id, "error": ["code": -32603, "message": text]])
        } catch {
            send(["jsonrpc": "2.0", "id": id, "error": ["code": -32603, "message": "Could not reach the box: \(error)"]])
        }
    }

    private func callConnect(_ id: Any, _ params: [String: Any]) async {
        for attempt in 0..<2 {
            do {
                return relay(id, try await call(try await openConnect(), "tools/call", params))
            } catch MCPUpstream.Failure.sessionExpired where attempt == 0 {
                connect = nil
            } catch {
                return reply(id, ToolFailure("Could not reach CocoonBox Connect: \(error)").result)
            }
        }
    }

    private func isConnector(_ name: String) async -> Bool {
        if boxTools.contains(where: { $0["name"] as? String == name }) { return false }
        await connectReady()
        return connectTools.contains { $0["name"] as? String == name }
    }

    private func startConnect() {
        guard connectWatch == nil else { return }
        connectFirst = Task { try? await pause(Self.connectWait) }
        connectWatch = Task { await self.watchConnect() }
    }

    // watchConnect lists the connector tools, then holds the door's stream and
    // lists them again on each list_changed; it reconnects with backoff. An api
    // without the door, or one that refuses the token, ends it with no connector tools.
    private func watchConnect() async {
        var delay = Duration.seconds(1)
        while !Task.isCancelled {
            let started = ContinuousClock.now
            do {
                let upstream = try await openConnect()
                try await listConnect(upstream)
                try await upstream.listen { message in await self.connectEvent(message) }
            } catch MCPUpstream.Failure.http(let status) where [401, 403, 404, 405].contains(status) {
                return loadedConnect()
            } catch MCPUpstream.Failure.sessionExpired {
                connect = nil
            } catch {}
            loadedConnect()
            if ContinuousClock.now - started > .seconds(30) { delay = .seconds(1) }
            try? await pause(delay)
            delay = min(delay * 2, .seconds(60))
        }
    }

    private func connectEvent(_ message: Data) async {
        guard let object = try? JSONSerialization.jsonObject(with: message) as? [String: Any],
              object["method"] as? String == "notifications/tools/list_changed", let connect else { return }
        try? await listConnect(connect)
    }

    // listConnect takes the door's tools, leaving out a name a control or box tool
    // has, and tells the client when they differ from what it last listed.
    private func listConnect(_ upstream: MCPUpstream) async throws {
        let answer = try await call(upstream, "tools/list", [:])
        guard let result = answer["result"] as? [String: Any] else { throw ToolFailure("The Connect door refused tools/list.") }
        let taken = ToolCatalog.controlNames.union(ToolCatalog.boxNames)
        let tools = result["tools"] as? [[String: Any]] ?? []
        connectTools = tools.filter { !taken.contains($0["name"] as? String ?? "") }
        for name in tools.compactMap({ $0["name"] as? String }) where taken.contains(name) {
            FileHandle.standardError.write(Data("cocoonbox: the connector tool \(name) is not served: a cocoonbox tool has that name\n".utf8))
        }
        loadedConnect()
        if let listedConnect, !ToolCatalog.same(listedConnect, connectTools) {
            send(["jsonrpc": "2.0", "method": "notifications/tools/list_changed"])
        }
    }

    private func connectReady() async {
        startConnect()
        await connectFirst?.value
    }

    private func loadedConnect() {
        connectFirst?.cancel()
    }

    // target returns the id of the box ref names, or of the default box when ref is empty.
    // An id passes as is, so the edge decides whether it is the user's.
    private func target(_ ref: String?) async throws -> String {
        if let ref, !ref.isEmpty {
            return try await resolve(ref)
        }
        if let pushed = link?.defaultBox, pushed != pinned {
            pinned = pushed
            defaultBox = try await resolve(pushed)
            if let box = defaultBox, listedBox, !ToolCatalog.same(toolsOf(box), boxTools) {
                boxTools = toolsOf(box)
                send(["jsonrpc": "2.0", "method": "notifications/tools/list_changed"])
            }
        }
        if let defaultBox { return defaultBox }
        if let pinned {
            defaultBox = try await resolve(pinned)
        } else {
            let list = try await listBoxes()
            guard let pick = list.last(where: { $0["state"] as? String == "running" }) ?? list.first, let box = pick["box_id"] as? String else {
                throw ToolFailure("You have no box yet. Call box_ensure to create it.")
            }
            defaultBox = box
        }
        return defaultBox ?? ""
    }

    private func resolve(_ ref: String) async throws -> String {
        if let box = ids[ref] { return box }
        if ref.hasPrefix("cb-") { return ref }
        _ = try await listBoxes()
        guard let box = ids[ref] else { throw ToolFailure("You have no box \"\(ref)\"; box_list lists your boxes.") }
        return box
    }

    private func listBoxes() async throws -> [[String: Any]] {
        let answer = try await call(try await openControl(), "tools/call", ["name": "box_list", "arguments": [:] as [String: Any]])
        guard let result = answer["result"] as? [String: Any], let list = (result["structuredContent"] as? [String: Any])?["boxes"] as? [[String: Any]] else {
            throw ToolFailure("Could not list your boxes.")
        }
        ids = [:]
        entries = [:]
        for entry in list {
            guard let box = entry["box_id"] as? String else { continue }
            ids[box] = box
            entries[box] = entry
            if let name = entry["name"] as? String, !name.isEmpty {
                ids[name] = box
                names[box] = name
            }
        }
        return list
    }

    // readyBox returns box's session, waking the box first when it is not
    // known to run; concurrent callers of one box share one wake.
    private func readyBox(_ box: String, progress: Any?, running: Bool = false) async throws -> MCPUpstream {
        if let upstream = sessions[box] { return upstream }
        if let task = opening[box] { return try await task.value }
        let progressToken = Progress(token: progress)
        let task = Task { running ? try await self.openBox(box) : try await self.wakeAndOpen(box, progress: progressToken.token) }
        opening[box] = task
        defer { opening[box] = nil }
        return try await task.value
    }

    private func wakeAndOpen(_ box: String, progress: Any?) async throws -> MCPUpstream {
        let status = try await status(box)
        if status["state"] as? String != "running" {
            guard wake else {
                throw ToolFailure("The box is \(status["state"] as? String ?? "absent"). Call box_ensure to start it.")
            }
            var params: [String: Any] = ["name": "box_ensure", "arguments": ["box": box, "wait": true]]
            if let progress { params["_meta"] = ["progressToken": progress] }
            let answer = try await call(try await openControl(), "tools/call", params)
            if let result = answer["result"] as? [String: Any], result["isError"] as? Bool == true {
                throw ToolFailure(result)
            }
        }
        return try await openBox(box)
    }

    private func openBox(_ box: String) async throws -> MCPUpstream {
        if let upstream = sessions[box] { return upstream }
        let upstream = try MCPUpstream(endpoint: endpoint, path: "/cocoonbox.mcp/\(box)", token: token)
        let instructions = try await initialize(upstream)
        sessions[box] = upstream
        boxInstructions[box] = instructions
        try? FileManager.default.createDirectory(at: cache.deletingLastPathComponent(), withIntermediateDirectories: true)
        try? Data(instructions.utf8).write(to: cacheFile(box, "instructions.txt"), options: .atomic)
        try? Data((entries[box]?["image"] as? String ?? "").utf8).write(to: cacheFile(box, "image"), options: .atomic)
        if let tools = (try? await call(upstream, "tools/list", [:]))?["result"] as? [String: Any], let list = tools["tools"] as? [[String: Any]], !list.isEmpty {
            ToolCatalog.saveBox(list, to: cacheFile(box))
            boxLists[box] = list
            if box == defaultBox, listedBox, !ToolCatalog.same(list, boxTools) {
                boxTools = list
                send(["jsonrpc": "2.0", "method": "notifications/tools/list_changed"])
            }
        }
        return upstream
    }

    // defaultTools are the default box's own tools: its list from this session, else its cached list, else the contract's names.
    private func defaultTools() async -> [[String: Any]] {
        guard let box = try? await target(nil) else { return ToolCatalog.boxFallback }
        return toolsOf(box)
    }

    private func toolsOf(_ box: String) -> [[String: Any]] {
        boxLists[box] ?? ToolCatalog.loadBox(from: cacheFile(box)) ?? ToolCatalog.boxFallback
    }

    private func instructionsText() async -> String {
        async let keys = listKeys()
        let text = await boxInstructionsText()
        let (list, link) = await keys
        guard let box = try? await target(nil), Self.loadsAtStart(toolsOf(box), "shell") else { return Self.withKeys(text, list, link: link) }
        shellKeys = (list, link)
        return text
    }

    private func withShellKeys(_ tool: [String: Any]) -> [String: Any] {
        guard let shellKeys, tool["name"] as? String == "shell" else { return tool }
        var tool = tool
        tool["description"] = Self.withKeys(tool["description"] as? String ?? "", shellKeys.keys, link: shellKeys.link)
        return tool
    }

    static func loadsAtStart(_ tools: [[String: Any]], _ name: String) -> Bool {
        let tool = tools.first { $0["name"] as? String == name }
        return (tool?["_meta"] as? [String: Any])?["anthropic/alwaysLoad"] as? Bool == true
    }

    // withKeys adds linkLine when link is set and it fits, then names as many of the user's keys as fit under maxInstructions.
    public static func withKeys(_ text: String, _ keys: [Cocoonbox_Control_V1_Key], link: Bool = false) -> String {
        let text = link && text.count + 1 + linkLine.count <= maxInstructions ? "\(text) \(linkLine)" : text
        let entries = keys.map { "\($0.name) for \($0.hosts.joined(separator: ", ")) as \(sentAs($0))" }
        for shown in stride(from: entries.count, through: 1, by: -1) {
            let more = shown < entries.count ? "; \(entries.count - shown) more in `cocoonbox keys ls`" : ""
            let full = "\(text) \(keysHead) \(entries.prefix(shown).joined(separator: "; "))\(more)."
            if full.count <= maxInstructions { return full }
        }
        return text
    }

    public static func sentAs(_ key: Cocoonbox_Control_V1_Key) -> String {
        let variable = "$" + key.name
        if !key.query.isEmpty { return "query ?\(key.query)=\(variable)" }
        if key.template.hasPrefix("Basic ") {
            return "\(key.header): Basic base64(\(key.template.dropFirst(6).replacingOccurrences(of: "{}", with: variable)))"
        }
        return "\(key.header): \(key.template.replacingOccurrences(of: "{}", with: variable))"
    }

    // listKeys reads the user's keys and whether a Link grant is active from the api, or neither when it does not answer within keysWait.
    private func listKeys() async -> ([Cocoonbox_Control_V1_Key], Bool) {
        guard let connection = try? BoxConnection(endpoint: endpoint, token: token) else { return ([], false) }
        defer { connection.close() }
        return await withTaskGroup(of: ([Cocoonbox_Control_V1_Key], Bool)?.self) { group in
            group.addTask {
                async let keys = try? connection.listKeys()
                let link = (try? await connection.listConnectors())?.contains { $0.kind == "link" && $0.status == "active" } ?? false
                return (await keys ?? [], link)
            }
            group.addTask {
                try? await pause(Self.keysWait)
                return nil
            }
            defer { group.cancelAll() }
            return await group.next().flatMap { $0 } ?? ([], false)
        }
    }

    private func boxInstructionsText() async -> String {
        guard let box = try? await target(nil) else { return Self.instructions }
        let plans = { (tools: [[String: Any]]) in tools.contains { $0["name"] as? String == "gui_plan" } }
        if sessions[box] == nil, let entry = await entry(box), entry["state"] as? String == "running",
           (try? String(contentsOf: cacheFile(box, "image"), encoding: .utf8)) != entry["image"] as? String {
            _ = try? await readyBox(box, progress: nil, running: true)
        }
        let own = boxInstructions[box] ?? (try? String(contentsOf: cacheFile(box, "instructions.txt"), encoding: .utf8)) ?? ""
        guard plans(toolsOf(box)), !own.isEmpty else { return Self.instructions }
        if (client["clientInfo"] as? [String: Any])?["name"] as? String == Self.codexClient { return Self.codexInstructions(own) }
        return "\(Self.instructions) The default box's own instructions follow. \(own)"
    }

    // codexInstructions is one line, since Codex copies a server's instructions into every tool's description.
    public static func codexInstructions(_ own: String) -> String {
        let machine = own.prefix { $0 != "\n" }.components(separatedBy: ". ").first ?? ""
        return "\(machine.hasSuffix(".") ? String(machine.dropLast()) : machine). It is the user's cocoonbox, not this Mac: GUI work in an app goes to gui_plan; commands go to shell."
    }

    // cacheFile keeps one box's tools and instructions apart from every other box's, since image families serve different ones.
    private func cacheFile(_ box: String, _ suffix: String = "json") -> URL {
        cache.deletingLastPathComponent().appendingPathComponent("\(cache.deletingPathExtension().lastPathComponent)-\(box).\(suffix)")
    }

    private func openConnect() async throws -> MCPUpstream {
        if let connect { return connect }
        let upstream = try MCPUpstream(endpoint: endpoint, path: "/cocoonbox.mcp/connect", token: token)
        try await initialize(upstream)
        connect = upstream
        return upstream
    }

    private func openControl() async throws -> MCPUpstream {
        if let control { return control }
        let upstream = try MCPUpstream(endpoint: endpoint, path: "/cocoonbox.mcp/control", token: token)
        try await initialize(upstream)
        control = upstream
        return upstream
    }

    // initialize opens an upstream session with the client's own initialize.
    @discardableResult
    private func initialize(_ upstream: MCPUpstream) async throws -> String {
        var params = client
        if params.isEmpty { params = ["protocolVersion": "2025-06-18", "capabilities": [:] as [String: Any], "clientInfo": ["name": "cocoonbox", "version": "0.1.0"]] }
        let answer = try await call(upstream, "initialize", params)
        if answer["error"] != nil { throw ToolFailure("The server refused the session: \(answer["error"] ?? "")") }
        Task { _ = try? await upstream.post(Data(#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.utf8), id: nil) }
        return (answer["result"] as? [String: Any])?["instructions"] as? String ?? ""
    }

    // entry is box's entry of the last box_list, else its box_status.
    private func entry(_ box: String) async -> [String: Any]? {
        if entries[box] == nil { entries[box] = try? await status(box) }
        return entries[box]
    }

    private func status(_ box: String) async throws -> [String: Any] {
        let answer = try await call(try await openControl(), "tools/call", ["name": "box_status", "arguments": ["box": box]])
        guard let result = answer["result"] as? [String: Any] else { throw ToolFailure("Could not read the box state.") }
        if result["isError"] as? Bool == true { throw ToolFailure(result) }
        guard let status = result["structuredContent"] as? [String: Any] else { throw ToolFailure("Could not read the box state.") }
        return status
    }

    private func forward(_ id: Any, _ params: [String: Any], to upstream: () async throws -> MCPUpstream) async {
        do {
            relay(id, try await call(try await upstream(), "tools/call", params))
        } catch MCPUpstream.Failure.sessionExpired {
            control = nil
            await forward(id, params, to: { try await self.openControl() })
        } catch {
            reply(id, ToolFailure("Could not reach cocoonbox: \(error)").result)
        }
    }

    private func call(_ upstream: MCPUpstream, _ method: String, _ params: [String: Any]) async throws -> [String: Any] {
        let id = nextID
        nextID += 1
        let output = output
        let body = try JSONSerialization.data(withJSONObject: ["jsonrpc": "2.0", "id": id, "method": method, "params": params])
        let answer = try await upstream.post(body, id: "\(id)") { notification in
            output(notification + Data("\n".utf8))
        }
        guard let answer, let object = try JSONSerialization.jsonObject(with: answer) as? [String: Any] else { throw MCPUpstream.Failure.noResponse }
        return object
    }

    private func relay(_ id: Any, _ answer: [String: Any]) {
        var message = answer
        message["id"] = id
        send(message)
    }

    private func reply(_ id: Any, _ result: [String: Any]) {
        send(["jsonrpc": "2.0", "id": id, "result": result])
    }

    private func send(_ message: [String: Any]) {
        guard let data = try? JSONSerialization.data(withJSONObject: message, options: [.withoutEscapingSlashes]) else { return }
        output(data + Data("\n".utf8))
    }
}

extension Bridge {
    // refuses says whether error may be the edge refusing a box id: a 403 or 404, or a box_status that names no box.
    static func refuses(_ error: any Error) -> Bool {
        switch error {
        case is ToolFailure, MCPUpstream.Failure.http(403), MCPUpstream.Failure.http(404): true
        default: false
        }
    }

    static func result(of error: any Error, box: String) -> [String: Any] {
        switch error {
        case let failure as ToolFailure: failure.result
        case let gone as BoxGone: ToolFailure("The box \(gone.name) was deleted; box_list lists your boxes.").result
        case MCPUpstream.Failure.http(403), MCPUpstream.Failure.http(404): ToolFailure("cocoonbox has no box \(box) for you; it may have been deleted. box_list lists your boxes.").result
        default: ToolFailure("Could not reach the box: \(error)").result
        }
    }
}

// Progress carries a client's progress token, a plain JSON value, into a task.
private struct Progress: @unchecked Sendable {
    let token: Any?
}

// ToolFailure is a tool result with isError, so the model reads the reason.
struct ToolFailure: Error {
    let result: [String: Any]

    init(_ text: String) {
        result = ["isError": true, "content": [["type": "text", "text": text]]]
    }

    init(_ result: [String: Any]) {
        self.result = result
    }
}
