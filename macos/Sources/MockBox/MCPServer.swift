import Foundation
import Network
import Synchronization

// FrontDoor listens on the public port like the edge does: HTTP/2 prior
// knowledge (gRPC) is spliced to the gRPC server, and HTTP/1.1 requests for
// /cocoonbox.mcp/<box id> are answered by MCPServer.
final class FrontDoor: Sendable {
    private let grpcPort: UInt16
    private let mcp: MCPServer
    private let queue = DispatchQueue(label: "mockbox.front-door")

    init(grpcPort: UInt16, mcp: MCPServer) {
        self.grpcPort = grpcPort
        self.mcp = mcp
    }

    func start(port: UInt16) async throws {
        let parameters = NWParameters.tcp
        parameters.requiredLocalEndpoint = .hostPort(host: "127.0.0.1", port: NWEndpoint.Port(rawValue: port)!)
        let listener = try NWListener(using: parameters)
        listener.newConnectionHandler = { [self] connection in
            connection.start(queue: queue)
            connection.receive(minimumIncompleteLength: 1, maximumLength: 64 << 10) { [self] data, _, _, _ in
                guard let data else { return connection.cancel() }
                if data.starts(with: Data("PRI * HTTP/2.0".utf8)) {
                    splice(connection, first: data)
                } else {
                    HTTPConnection(connection: connection, mcp: mcp).run(first: data)
                }
            }
        }
        try await withCheckedThrowingContinuation { (started: CheckedContinuation<Void, any Error>) in
            listener.stateUpdateHandler = { state in
                switch state {
                case .ready: started.resume()
                case .failed(let error), .waiting(let error): started.resume(throwing: error)
                default: return
                }
                listener.stateUpdateHandler = nil
            }
            listener.start(queue: queue)
        }
    }

    private func splice(_ client: NWConnection, first: Data) {
        let server = NWConnection(host: "127.0.0.1", port: NWEndpoint.Port(rawValue: grpcPort)!, using: .tcp)
        server.start(queue: queue)
        server.send(content: first, completion: .idempotent)
        pump(from: client, to: server)
        pump(from: server, to: client)
    }

    private func pump(from source: NWConnection, to sink: NWConnection) {
        source.receive(minimumIncompleteLength: 1, maximumLength: 256 << 10) { [self] data, _, done, error in
            if let data, !data.isEmpty {
                sink.send(content: data, completion: .idempotent)
            }
            if done || error != nil {
                source.cancel()
                sink.cancel()
                return
            }
            pump(from: source, to: sink)
        }
    }
}

// HTTPConnection serves HTTP/1.1 requests with Content-Length bodies on one connection.
private final class HTTPConnection: @unchecked Sendable {
    private let connection: NWConnection
    private let mcp: MCPServer
    private var buffer = Data()

    init(connection: NWConnection, mcp: MCPServer) {
        self.connection = connection
        self.mcp = mcp
    }

    func run(first: Data) {
        buffer.append(first)
        drain()
    }

    private func drain() {
        guard let headerEnd = buffer.range(of: Data("\r\n\r\n".utf8)) else { return read() }
        let head = String(decoding: buffer[..<headerEnd.lowerBound], as: UTF8.self)
        var lines = head.components(separatedBy: "\r\n")
        let requestLine = lines.removeFirst().split(separator: " ")
        var headers: [String: String] = [:]
        for line in lines {
            guard let colon = line.firstIndex(of: ":") else { continue }
            headers[line[..<colon].lowercased()] = line[line.index(after: colon)...].trimmingCharacters(in: .whitespaces)
        }
        let length = Int(headers["content-length"] ?? "0") ?? 0
        let bodyStart = headerEnd.upperBound
        guard buffer.count - bodyStart >= length else { return read() }
        let body = buffer[bodyStart..<bodyStart + length]
        buffer = Data(buffer[(bodyStart + length)...])
        let method = requestLine.first.map(String.init) ?? ""
        let path = requestLine.count > 1 ? String(requestLine[1]) : ""
        Task {
            let response = await mcp.handle(method: method, path: path, headers: headers, body: Data(body))
            guard let events = response.events else {
                connection.send(content: response.encoded(), completion: .contentProcessed { [self] _ in drain() })
                return
            }
            connection.send(content: response.encoded(), completion: .idempotent)
            for await event in events {
                connection.send(content: Data("data: ".utf8) + event + Data("\n\n".utf8), completion: .idempotent)
            }
            connection.send(content: nil, contentContext: .finalMessage, isComplete: true, completion: .contentProcessed { [connection] _ in connection.cancel() })
        }
    }

    private func read() {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 256 << 10) { [self] data, _, done, error in
            if let data { buffer.append(data) }
            if done || error != nil {
                connection.cancel()
                return
            }
            drain()
        }
    }
}

// HTTPResponse is a whole body, or with `events` an SSE stream written as it
// arrives on a connection that closes after it.
struct HTTPResponse {
    var status: Int
    var reason: String
    var headers: [String: String] = [:]
    var body = Data()
    var events: AsyncStream<Data>?

    func encoded() -> Data {
        if events != nil {
            var head = "HTTP/1.1 \(status) \(reason)\r\nConnection: close\r\nCache-Control: no-cache\r\n"
            for (name, value) in headers { head += "\(name): \(value)\r\n" }
            return Data((head + "\r\n").utf8)
        }
        var head = "HTTP/1.1 \(status) \(reason)\r\nContent-Length: \(body.count)\r\n"
        for (name, value) in headers { head += "\(name): \(value)\r\n" }
        return Data((head + "\r\n").utf8) + body
    }
}

// MCPServer answers MCP Streamable HTTP with JSON responses, a tool call with
// a progress token as an SSE stream (no server stream). Every session is the owner's, which the box does not track as an agent.
// It logs one line per request with the auth verdict, never the token.
final class MCPServer: Sendable {
    static let prefix = "/cocoonbox.mcp/"
    nonisolated(unsafe) static let tools: [[String: Any]] = [
        tool("shell", "Run a shell command on the box.", ["command": "string"]),
        tool("read_file", "Read a file on the box.", ["path": "string"]),
        tool("write_file", "Write a file on the box.", ["path": "string", "content": "string"]),
        tool("edit_file", "Replace text in a file on the box.", ["path": "string", "old": "string", "new": "string"]),
        tool("list_dir", "List a directory on the box.", ["path": "string"]),
        tool("browser_navigate", "Open a URL in the box's browser.", ["url": "string"]),
        tool("browser_snapshot", "Snapshot the page.", [:]),
        tool("browser_click", "Click an element.", ["ref": "string"]),
        tool("browser_type", "Type into an element.", ["ref": "string", "text": "string"]),
        tool("browser_read", "Read the page text.", [:]),
        tool("computer_state", "Describe the desktop.", [:]),
        tool("computer_act", "Click, type or press keys on the desktop.", ["action": "string", "x": "integer", "y": "integer", "text": "string"]),
        tool("screenshot", "Take a PNG screenshot of the desktop.", ["max_edge": "integer"]),
    ]
    nonisolated(unsafe) static let scriptTools: [[String: Any]] = (tools + [tool("gui_plan", "Run a GUI plan on the desktop.", ["steps": "array"]), tool("script_run", "Run a script on the desktop.", ["code": "string"])]).map { tool in
        ["shell", "gui_plan", "script_run"].contains(tool["name"] as? String) ? tool.merging(["_meta": ["anthropic/alwaysLoad": true]]) { $1 } : tool
    }
    nonisolated(unsafe) static let planTools = scriptTools.filter { $0["name"] as? String != "script_run" }
    nonisolated(unsafe) static let androidTools: [[String: Any]] = (tools.filter { !($0["name"] as? String ?? "").hasPrefix("browser_") && !["computer_state", "computer_act", "screenshot"].contains($0["name"] as? String) } + [
        tool("computer_state", "List the phone screen's UI elements.", [:]),
        tool("computer_act", "Tap, swipe, type or press keys on the phone.", ["action": "string", "element": "integer", "text": "string"]),
        tool("screenshot", "Take a PNG screenshot of the phone.", ["max_edge": "integer"]),
        tool("gui_plan", "Run an Android GUI plan on the phone.", ["steps": "array"]),
        tool("script_run", "Run a script on the phone.", ["code": "string"]),
    ]).map { tool in
        ["shell", "gui_plan", "script_run"].contains(tool["name"] as? String) ? tool.merging(["_meta": ["anthropic/alwaysLoad": true]]) { $1 } : tool
    }
    static let androidInstructions = "These tools act on an Android 16 phone (720x1280) that runs in a cloud computer: Ubuntu 24.04 with no desktop. shell has adb."

    private let world: MockWorld
    private let token: String
    private let sessions = Mutex<Set<String>>([])
    private let control: ControlMCP
    private let oauth: MockOAuth

    init(world: MockWorld, token: String, issuer: String) {
        self.world = world
        self.token = token
        control = ControlMCP(world: world)
        oauth = MockOAuth(token: token, issuer: issuer)
    }

    private static func tool(_ name: String, _ description: String, _ properties: [String: String]) -> [String: Any] {
        [
            "name": name,
            "description": description,
            "inputSchema": ["type": "object", "properties": properties.mapValues { ["type": $0] }],
        ]
    }

    func handle(method: String, path: String, headers: [String: String], body: Data) async -> HTTPResponse {
        let authorization = headers["authorization"] ?? ""
        let auth = authorization == "Bearer \(token)" ? "ok" : authorization.contains("${") ? "unexpanded" : authorization.isEmpty ? "missing" : "bad"
        var line = "mcp \(method) \(path) auth=\(auth) ua=\(headers["user-agent"]?.split(separator: " ").first ?? "-")"
        defer { print(line) }
        if let response = oauth.handle(method: method, path: path, headers: headers, body: body, line: &line) {
            return response
        }
        let target = path.split(separator: "?").first.map(String.init) ?? ""
        let boxID = target.hasPrefix(Self.prefix) && target != ControlMCP.path && target != MockConnect.path ? String(target.dropFirst(Self.prefix.count)) : ""
        guard target == ControlMCP.path || target == MockConnect.path || !boxID.isEmpty else {
            return HTTPResponse(status: 404, reason: "Not Found")
        }
        guard auth == "ok" else {
            return HTTPResponse(status: 401, reason: "Unauthorized")
        }
        if target == MockConnect.path {
            return await world.connect.handle(method: method, headers: headers, body: body, line: &line)
        }
        if target == ControlMCP.path {
            guard method == "POST" else { return HTTPResponse(status: 405, reason: "Method Not Allowed", headers: ["Allow": "POST"]) }
            guard let message = (try? JSONSerialization.jsonObject(with: body)) as? [String: Any] else {
                return HTTPResponse(status: 400, reason: "Bad Request")
            }
            return await control.handle(message, line: &line)
        }
        guard let box = await world.box(boxID), box.id == boxID else {
            return HTTPResponse(status: 404, reason: "Not Found")
        }
        guard box.state == .running else {
            return HTTPResponse(status: 503, reason: "Service Unavailable")
        }
        let sessionID = headers["mcp-session-id"] ?? ""
        switch method {
        case "POST":
            break
        case "DELETE":
            sessions.withLock { _ = $0.remove(sessionID) }
            return HTTPResponse(status: 200, reason: "OK")
        default:
            return HTTPResponse(status: 405, reason: "Method Not Allowed", headers: ["Allow": "POST, DELETE"])
        }
        guard let message = (try? JSONSerialization.jsonObject(with: body)) as? [String: Any] else {
            return HTTPResponse(status: 400, reason: "Bad Request")
        }
        let rpc = message["method"] as? String ?? ""
        let params = message["params"] as? [String: Any] ?? [:]
        line += " rpc=\(rpc)"
        guard let id = message["id"] else {
            return HTTPResponse(status: 202, reason: "Accepted")
        }
        var headersOut: [String: String] = ["Content-Type": "application/json"]
        let result: [String: Any]
        switch rpc {
        case "initialize":
            let info = params["clientInfo"] as? [String: Any]
            let client = info?["name"] as? String ?? "unknown"
            let newSession = UUID().uuidString.lowercased()
            sessions.withLock { _ = $0.insert(newSession) }
            headersOut["Mcp-Session-Id"] = newSession
            line += " client=\(client) protocol=\(params["protocolVersion"] as? String ?? "-")"
            result = [
                "protocolVersion": params["protocolVersion"] as? String ?? "2025-06-18",
                "capabilities": ["tools": ["listChanged": false]],
                "serverInfo": ["name": "cocoonbox-mock", "version": "0.1"],
                "instructions": box.image.hasPrefix("cbdroid:") ? Self.androidInstructions : box.image.hasPrefix("cbcore:") ? "Mock cbcore box: write one script with script_run for a multi-step task." : box.image.hasPrefix("cbdesk:") ? "Mock cbdesk box." : "Mock box that runs GUI plans.",
            ]
        case "tools/list":
            result = ["tools": box.image.hasPrefix("cbdroid:") ? Self.androidTools : box.image.hasPrefix("cbcore:") ? Self.scriptTools : box.image.hasPrefix("cbdesk:") ? Self.tools : Self.planTools]
        case "tools/call":
            guard sessions.withLock({ $0.contains(sessionID) }) else {
                return HTTPResponse(status: 404, reason: "Not Found")
            }
            let name = params["name"] as? String ?? ""
            let arguments = params["arguments"].flatMap { try? JSONSerialization.data(withJSONObject: $0) }.map { String(decoding: $0, as: UTF8.self) } ?? "{}"
            line += " tool=\(name)"
            let outcome = await world.externalCall(agent: "", tool: name, arguments: arguments, on: box.name)
            var content: [[String: Any]] = [["type": "text", "text": outcome.text]]
            if let image = outcome.image {
                content.append(["type": "image", "data": image.base64EncodedString(), "mimeType": "image/png"])
            }
            result = ["content": content, "isError": outcome.isError]
            if let token = (params["_meta"] as? [String: Any])?["progressToken"] {
                let progress: [String: Any] = ["jsonrpc": "2.0", "method": "notifications/progress", "params": ["progressToken": token, "progress": 1, "message": "running for 25 s"]]
                let events = [progress, ["jsonrpc": "2.0", "id": id, "result": result]].compactMap { try? JSONSerialization.data(withJSONObject: $0) }
                return HTTPResponse(status: 200, reason: "OK", headers: ["Content-Type": "text/event-stream"], events: AsyncStream { stream in
                    events.forEach { stream.yield($0) }
                    stream.finish()
                })
            }
        case "ping":
            result = [:]
        default:
            let error: [String: Any] = ["jsonrpc": "2.0", "id": id, "error": ["code": -32601, "message": "method not found: \(rpc)"]]
            return HTTPResponse(status: 200, reason: "OK", headers: headersOut, body: (try? JSONSerialization.data(withJSONObject: error)) ?? Data())
        }
        let reply: [String: Any] = ["jsonrpc": "2.0", "id": id, "result": result]
        return HTTPResponse(status: 200, reason: "OK", headers: headersOut, body: (try? JSONSerialization.data(withJSONObject: reply)) ?? Data())
    }
}
