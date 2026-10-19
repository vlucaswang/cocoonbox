import CocoonBoxKit
import Foundation
import GRPCCore

// ControlMCP plays the api's control MCP (contracts.md "Control MCP") on
// MockWorld: the lifecycle tools of the user's boxes, their progress and their errors.
final class ControlMCP: Sendable {
    static let path = "/cocoonbox.mcp/control"
    static let versions = ["2025-06-18", "2025-11-25"]

    nonisolated(unsafe) static let box: [String: Any] = ["type": "string", "description": "The box's name or id; without it, your first box."]

    nonisolated(unsafe) static let tools: [[String: Any]] = [
        tool("box_list", "List your cocoonbox boxes with their names, states and images.", [:], []),
        tool("box_status", "Show the state of your cocoonbox box: running or hibernated, its image, whether an upgrade is available, and when your files were last saved.", ["box": box], []),
        tool("box_ensure", "Start or wake your cocoonbox box and wait until it is running. Waking takes a few seconds; creating a new box takes longer.", ["box": box, "wait": ["type": "boolean", "default": true]], []),
        tool("box_create", "Create another cocoonbox box with this name and wait until it is running. Your other boxes stay as they are; only a limited number may run at once.", ["name": ["type": "string"], "image": ["type": "string", "description": "An image family, such as cbdroid for an Android phone, or an image; without it, your usual desktop."], "wait": ["type": "boolean", "default": true]], ["name"]),
        tool("box_hibernate", "Hibernate your cocoonbox box. Running programs and memory are kept and come back on the next wake.", ["box": box], []),
        tool("box_save", "Save your files, logins and agent history from the box to storage now. On a desktop box the browser closes during the save.", ["box": box], []),
        tool("box_upgrade", "Move your box to the latest image of its kind. Running programs restart; files, logins and agent history are kept. Requires confirm: true.", ["box": box, "confirm": ["type": "boolean"]], ["confirm"]),
        tool("box_delete", "Delete one of your cocoonbox boxes. A running box is saved first; its saved state is kept for 7 days, then removed. Requires the box and confirm: true.", ["box": box, "confirm": ["type": "boolean"]], ["box", "confirm"]),
        tool("usage", "Show the model usage gw metered for your account, all your boxes together: requests, tokens and cost in micro-dollars today and this month (UTC), by model, with your daily cost cap (-1 unlimited) and what is left of it today. Your own model client's tokens are not in it.", [:], []),
    ]

    private let world: MockWorld

    init(world: MockWorld) {
        self.world = world
    }

    private static func tool(_ name: String, _ description: String, _ properties: [String: Any], _ required: [String]) -> [String: Any] {
        ["name": name, "description": description, "inputSchema": ["type": "object", "properties": properties, "required": required]]
    }

    func handle(_ message: [String: Any], line: inout String) async -> HTTPResponse {
        let rpc = message["method"] as? String ?? ""
        let params = message["params"] as? [String: Any] ?? [:]
        line += " control rpc=\(rpc)"
        guard let id = message["id"] else { return HTTPResponse(status: 202, reason: "Accepted") }
        switch rpc {
        case "initialize":
            let asked = params["protocolVersion"] as? String ?? ""
            var response = Self.reply(id, [
                "protocolVersion": Self.versions.contains(asked) ? asked : Self.versions[1],
                "capabilities": ["tools": [:] as [String: Any]],
                "serverInfo": ["name": "cocoonbox-control", "version": "mock"],
            ])
            response.headers["Mcp-Session-Id"] = UUID().uuidString.lowercased()
            return response
        case "tools/list":
            return Self.reply(id, ["tools": Self.tools])
        case "ping":
            return Self.reply(id, [:])
        case "tools/call":
            let name = params["name"] as? String ?? ""
            let arguments = params["arguments"] as? [String: Any] ?? [:]
            let token = (params["_meta"] as? [String: Any])?["progressToken"]
            line += " tool=\(name)"
            return await call(name, arguments, id: id, token: token)
        default:
            return Self.error(id, -32601, "method not found: \(rpc)")
        }
    }

    private func call(_ name: String, _ arguments: [String: Any], id: Any, token: Any?) async -> HTTPResponse {
        let ref = arguments["box"] as? String ?? ""
        if name == "box_list" {
            let boxes = await world.boxes.map { ["box_id": $0.id, "name": $0.name, "state": Self.state($0.state), "image": $0.image, "last_saved_at_ms": $0.lastSavedAtMs] as [String: Any] }
            return Self.reply(id, Self.success(["boxes": boxes]))
        }
        if name == "box_create" {
            do {
                let response = try await world.create(name: arguments["name"] as? String ?? "", image: arguments["image"] as? String ?? "")
                guard arguments["wait"] as? Bool ?? true else {
                    return Self.reply(id, Self.success(["box_id": response.box.id, "name": response.box.name, "state": Self.state(response.box.state), "operation_id": response.operation.id]))
                }
                return await wait(response.operation.id, box: response.box.id, id: id, token: token, verb: "Create") { box, op in
                    ["box_id": box.id, "name": box.name, "state": Self.state(box.state), "operation_id": op]
                }
            } catch let error as RPCError {
                return Self.reply(id, Self.failure(error.message))
            } catch {
                return Self.error(id, -32603, "\(error)")
            }
        }
        if name == "usage" {
            return Self.reply(id, Self.success(MockWorld.usage.object))
        }
        if name == "box_delete", ref.isEmpty {
            return Self.reply(id, Self.failure("Name the box to delete in box."))
        }
        guard let box = await world.box(ref) else {
            return Self.reply(id, Self.failure("You have no box \"\(ref)\"; box_list lists your boxes."))
        }
        switch name {
        case "box_status":
            return Self.reply(id, Self.success(status(box)))
        case "box_ensure":
            let response: Cocoonbox_Control_V1_EnsureResponse
            do {
                response = try await world.ensure(box.id)
            } catch let error as RPCError {
                return Self.reply(id, Self.failure(error.message))
            } catch {
                return Self.error(id, -32603, "\(error)")
            }
            guard response.hasOperation, !response.operation.id.isEmpty, arguments["wait"] as? Bool ?? true else {
                return Self.reply(id, Self.success(["box_id": box.id, "state": Self.state(response.box.state), "operation_id": response.operation.id]))
            }
            return await wait(response.operation.id, box: box.id, id: id, token: token, verb: "Ensure") { box, op in
                ["box_id": box.id, "state": Self.state(box.state), "operation_id": op]
            }
        case "box_hibernate", "box_save":
            guard box.state == .running else { return Self.reply(id, Self.failure("The box is \(Self.state(box.state)).")) }
            let hibernate = name == "box_hibernate"
            let op = hibernate ? await world.hibernate(box.id) : await world.saveState(box.id)
            return await wait(op.id, box: box.id, id: id, token: token, verb: hibernate ? "Hibernate" : "Save") { box, op in
                hibernate
                    ? ["box_id": box.id, "state": Self.state(box.state), "operation_id": op]
                    : ["box_id": box.id, "last_saved_at_ms": box.lastSavedAtMs, "operation_id": op, "bytes": 48 << 20]
            }
        case "box_upgrade":
            guard arguments["confirm"] as? Bool == true else {
                return Self.reply(id, Self.failure("Upgrading restarts running programs; files, logins and agent history are kept. Call again with confirm: true."))
            }
            guard box.latestImage != box.image else { return Self.reply(id, Self.failure("Already on the latest image.")) }
            let op = await world.rebase(box.id, image: box.latestImage)
            return await wait(op.id, box: box.id, id: id, token: token, verb: "Upgrade") { box, op in
                ["box_id": box.id, "state": Self.state(box.state), "image": box.image, "operation_id": op]
            }
        case "box_delete":
            guard arguments["confirm"] as? Bool == true else {
                return Self.reply(id, Self.failure("Deleting removes the box and stops its running programs; its last saved state is kept for 7 days, then removed. Call again with confirm: true."))
            }
            let op: Cocoonbox_Control_V1_Operation
            do {
                op = try await world.delete(box: box.id)
            } catch {
                return Self.error(id, -32603, "\(error)")
            }
            let deleted = box.id
            return await wait(op.id, box: box.id, id: id, token: token, verb: "Delete") { _, op in
                ["box_id": deleted, "operation_id": op]
            }
        default:
            return Self.error(id, -32602, "unknown tool \(name)")
        }
    }

    // wait follows the operation to its end, as progress notifications when the caller sent a token.
    private func wait(_ op: String, box boxID: String, id: Any, token: Any?, verb: String, result: @escaping @Sendable (Box, String) -> [String: Any]) async -> HTTPResponse {
        let (stream, continuation) = AsyncStream<Data>.makeStream()
        let world = world
        let ids = JSONValues(id: id, token: token)
        let task = Task {
            let id = ids.id
            let token = ids.token
            var index = 0
            var final: [String: Any] = [:]
            if let (replay, live) = try? await world.watch(operation: op, after: "") {
                var events = replay
                if let live {
                    for await event in live { events.append(event) }
                }
                for event in events {
                    index += 1
                    if let token {
                        var message = "\(Self.phase(event.phase)): \(event.detail)"
                        if event.totalBytes > 0 { message += " (\(event.doneBytes)/\(event.totalBytes) bytes)" }
                        continuation.yield(Self.encode(["jsonrpc": "2.0", "method": "notifications/progress", "params": ["progressToken": token, "progress": index, "message": message]]))
                    }
                    if event.phase == .failed { final = Self.failure("\(verb) failed: \(event.detail)") }
                }
            }
            if final.isEmpty { final = Self.success(result(await world.box(boxID) ?? Box(), op)) }
            continuation.yield(Self.encode(["jsonrpc": "2.0", "id": id, "result": final]))
            continuation.finish()
        }
        if token != nil {
            return HTTPResponse(status: 200, reason: "OK", headers: ["Content-Type": "text/event-stream"], events: stream)
        }
        _ = await task.value
        var last = Data()
        for await chunk in stream { last = chunk }
        return HTTPResponse(status: 200, reason: "OK", headers: ["Content-Type": "application/json"], body: last)
    }

    // JSONValues carries a request id and progress token, plain JSON values, into the watching task.
    private struct JSONValues: @unchecked Sendable {
        let id: Any
        let token: Any?
    }

    private func status(_ box: Box) -> [String: Any] {
        [
            "box_id": box.id, "name": box.name, "state": Self.state(box.state), "image": box.image, "latest_image": box.latestImage,
            "upgrade_available": !box.latestImage.isEmpty && box.latestImage != box.image, "last_saved_at_ms": box.lastSavedAtMs,
        ]
    }

    private static func success(_ object: [String: Any]) -> [String: Any] {
        ["structuredContent": object, "content": [["type": "text", "text": String(decoding: encode(object), as: UTF8.self)]]]
    }

    private static func failure(_ text: String) -> [String: Any] {
        ["isError": true, "content": [["type": "text", "text": text]]]
    }

    private static func state(_ state: Cocoonbox_Control_V1_BoxState) -> String {
        switch state {
        case .starting: "starting"
        case .running: "running"
        case .hibernating: "hibernating"
        case .hibernated: "hibernated"
        case .failed: "failed"
        default: "absent"
        }
    }

    private static func phase(_ phase: Cocoonbox_Control_V1_Phase) -> String {
        switch phase {
        case .creating: "creating"
        case .waking: "waking"
        case .snapshotting: "snapshotting"
        case .saving: "saving"
        case .restoring: "restoring"
        case .restoringFiles: "restoring_files"
        case .deleting: "deleting"
        case .done: "done"
        case .failed: "failed"
        default: "unspecified"
        }
    }

    private static func encode(_ object: [String: Any]) -> Data {
        (try? JSONSerialization.data(withJSONObject: object, options: [.sortedKeys, .withoutEscapingSlashes])) ?? Data()
    }

    private static func reply(_ id: Any, _ result: [String: Any]) -> HTTPResponse {
        HTTPResponse(status: 200, reason: "OK", headers: ["Content-Type": "application/json"], body: encode(["jsonrpc": "2.0", "id": id, "result": result]))
    }

    private static func error(_ id: Any, _ code: Int, _ message: String) -> HTTPResponse {
        HTTPResponse(status: 200, reason: "OK", headers: ["Content-Type": "application/json"], body: encode(["jsonrpc": "2.0", "id": id, "error": ["code": code, "message": message]]))
    }
}
