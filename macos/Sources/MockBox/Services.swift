import CocoonBoxKit
import Foundation
import GRPCCore
import RFBTestServer

struct AuthCheck: ServerInterceptor {
    let token: String
    let world: MockWorld

    func intercept<Input: Sendable, Output: Sendable>(
        request: StreamingServerRequest<Input>,
        context: ServerContext,
        next: @Sendable (StreamingServerRequest<Input>, ServerContext) async throws -> StreamingServerResponse<Output>
    ) async throws -> StreamingServerResponse<Output> {
        guard request.metadata[stringValues: "authorization"].first(where: { _ in true }) == "Bearer \(token)" else {
            throw RPCError(code: .unauthenticated, message: "bad or missing user token")
        }
        if context.descriptor.service.package == "cocoonbox.agent.v1" {
            let box = request.metadata[stringValues: "x-cocoonbox-box"].first(where: { _ in true })
            guard let box, await world.box(box)?.id == box else {
                throw MockWorld.noBox
            }
        }
        return try await next(request, context)
    }
}

struct BoxesService: Cocoonbox_Control_V1_Boxes.SimpleServiceProtocol {
    let world: MockWorld

    func ensure(request: Cocoonbox_Control_V1_EnsureRequest, context: ServerContext) async throws -> Cocoonbox_Control_V1_EnsureResponse {
        try await world.ensure(request.boxID)
    }

    func create(request: Cocoonbox_Control_V1_CreateBoxRequest, context: ServerContext) async throws -> Cocoonbox_Control_V1_EnsureResponse {
        try await world.create(name: request.name, image: request.image)
    }

    func rename(request: Cocoonbox_Control_V1_RenameBoxRequest, context: ServerContext) async throws -> Cocoonbox_Control_V1_Box {
        try await world.rename(box: request.boxID, name: request.name)
    }

    func delete(request: Cocoonbox_Control_V1_BoxRef, context: ServerContext) async throws -> Cocoonbox_Control_V1_Operation {
        try await world.delete(box: request.boxID)
    }

    func get(request: Cocoonbox_Control_V1_GetBoxRequest, context: ServerContext) async throws -> Cocoonbox_Control_V1_Box {
        try await owned(request.boxID)
    }

    func list(request: Cocoonbox_Control_V1_ListBoxesRequest, context: ServerContext) async throws -> Cocoonbox_Control_V1_ListBoxesResponse {
        let boxes = await world.boxes
        return .with {
            $0.boxes = boxes
            $0.offeredImages = MockWorld.offeredImages
        }
    }

    func watch(request: Cocoonbox_Control_V1_WatchBoxesRequest, response: RPCWriter<Cocoonbox_Control_V1_ListBoxesResponse>, context: ServerContext) async throws {
        let (boxes, changes) = await world.watchBoxes()
        try await response.write(.with {
            $0.boxes = boxes
            $0.offeredImages = MockWorld.offeredImages
        })
        for await boxes in changes {
            try await response.write(.with {
                $0.boxes = boxes
                $0.offeredImages = MockWorld.offeredImages
            })
        }
    }

    func hibernate(request: Cocoonbox_Control_V1_BoxRef, context: ServerContext) async throws -> Cocoonbox_Control_V1_Operation {
        await world.hibernate(try await owned(request.boxID).id)
    }

    func saveState(request: Cocoonbox_Control_V1_BoxRef, context: ServerContext) async throws -> Cocoonbox_Control_V1_Operation {
        await world.saveState(try await owned(request.boxID).id)
    }

    func rebase(request: Cocoonbox_Control_V1_RebaseRequest, context: ServerContext) async throws -> Cocoonbox_Control_V1_Operation {
        await world.rebase(try await owned(request.boxID).id, image: request.image)
    }

    func listSnapshots(request: Cocoonbox_Control_V1_BoxRef, context: ServerContext) async throws -> Cocoonbox_Control_V1_ListSnapshotsResponse {
        let box = try await owned(request.boxID)
        let snapshots = await world.snapshots(of: box.id)
        return .with { $0.snapshots = snapshots }
    }

    func browseSnapshot(request: Cocoonbox_Control_V1_BrowseSnapshotRequest, context: ServerContext) async throws -> Cocoonbox_Control_V1_BrowseSnapshotResponse {
        try await world.browse(box: try await owned(request.boxID).id, snapshot: request.snapshotID, dir: request.dir)
    }

    func restorePaths(request: Cocoonbox_Control_V1_RestorePathsRequest, context: ServerContext) async throws -> Cocoonbox_Control_V1_Operation {
        try await world.restorePaths(box: try await owned(request.boxID).id, snapshot: request.snapshotID, paths: request.paths)
    }

    func setWakeSchedule(request: Cocoonbox_Control_V1_SetWakeScheduleRequest, context: ServerContext) async throws -> Cocoonbox_Control_V1_WakeSchedule {
        try await world.setWakeSchedule(box: try await owned(request.boxID).id, request)
    }

    func listWakeSchedules(request: Cocoonbox_Control_V1_ListWakeSchedulesRequest, context: ServerContext) async throws -> Cocoonbox_Control_V1_ListWakeSchedulesResponse {
        let schedules = await world.wakeSchedules
        return .with { $0.schedules = schedules }
    }

    func clearWakeSchedule(request: Cocoonbox_Control_V1_BoxRef, context: ServerContext) async throws -> Cocoonbox_Control_V1_ClearWakeScheduleResponse {
        try await world.clearWakeSchedule(box: try await owned(request.boxID).id)
        return Cocoonbox_Control_V1_ClearWakeScheduleResponse()
    }

    func usage(request: Cocoonbox_Control_V1_UsageRequest, context: ServerContext) async throws -> Cocoonbox_Control_V1_UsageResponse {
        MockWorld.usage
    }

    private func owned(_ boxID: String) async throws -> Cocoonbox_Control_V1_Box {
        guard let box = await world.box(boxID), box.id == boxID else { throw MockWorld.noBox }
        return box
    }

    func watchOperation(request: Cocoonbox_Control_V1_WatchOperationRequest, response: RPCWriter<Cocoonbox_Control_V1_OperationEvent>, context: ServerContext) async throws {
        let (replay, live) = try await world.watch(operation: request.operationID, after: request.fromOffsetKey)
        for event in replay {
            try await response.write(event)
        }
        guard let live else { return }
        for await event in live {
            try await response.write(event)
        }
    }
}

struct AgentsService: Cocoonbox_Agent_V1_Agents.SimpleServiceProtocol {
    let world: MockWorld

    func create(request: Cocoonbox_Agent_V1_CreateAgentRequest, context: ServerContext) async throws -> Cocoonbox_Agent_V1_Agent {
        await world.createAgent(name: request.name, nonce: request.clientNonce, model: request.mainModel)
    }

    func list(request: Cocoonbox_Agent_V1_ListAgentsRequest, context: ServerContext) async throws -> Cocoonbox_Agent_V1_ListAgentsResponse {
        let agents = await world.listAgents()
        return .with { $0.agents = agents }
    }

    func send(request: Cocoonbox_Agent_V1_SendRequest, context: ServerContext) async throws -> Cocoonbox_Agent_V1_SendResponse {
        let turn = try await world.send(agent: request.agentID, text: request.text, nonce: request.clientNonce)
        return .with { $0.turnID = turn }
    }

    func transcript(request: Cocoonbox_Agent_V1_TranscriptRequest, context: ServerContext) async throws -> Cocoonbox_Agent_V1_TranscriptPage {
        let limit = request.limit == 0 ? 200 : Int(request.limit)
        let (entries, hasMore) = await world.transcript(agent: request.agentID, after: request.afterSeq, limit: limit)
        return .with {
            $0.entries = entries
            $0.hasMore_p = hasMore
        }
    }

    func respond(request: Cocoonbox_Agent_V1_RespondRequest, context: ServerContext) async throws -> Cocoonbox_Agent_V1_RespondResponse {
        try await world.respond(request)
        return .init()
    }

    func cancel(request: Cocoonbox_Agent_V1_CancelRequest, context: ServerContext) async throws -> Cocoonbox_Agent_V1_CancelResponse {
        await world.cancel(agent: request.agentID)
        return .init()
    }

    func rename(request: Cocoonbox_Agent_V1_RenameAgentRequest, context: ServerContext) async throws -> Cocoonbox_Agent_V1_Agent {
        try await world.rename(agent: request.agentID, name: request.name)
    }

    func delete(request: Cocoonbox_Agent_V1_DeleteAgentRequest, context: ServerContext) async throws -> Cocoonbox_Agent_V1_DeleteAgentResponse {
        try await world.delete(agent: request.agentID)
        return .init()
    }

    func end(request: Cocoonbox_Agent_V1_EndRequest, context: ServerContext) async throws -> Cocoonbox_Agent_V1_EndResponse {
        try await world.end(request)
        return .init()
    }
}

struct SharesService: Cocoonbox_Control_V1_Shares.SimpleServiceProtocol {
    let world: MockWorld

    func create(request: Cocoonbox_Control_V1_CreateShareRequest, context: ServerContext) async throws -> Cocoonbox_Control_V1_ShareSecret {
        try await world.createShare(request)
    }

    func list(request: Cocoonbox_Control_V1_ListSharesRequest, context: ServerContext) async throws -> Cocoonbox_Control_V1_ListSharesResponse {
        let shares = await world.listShares()
        return .with { $0.shares = shares }
    }

    func setWrite(request: Cocoonbox_Control_V1_SetShareWriteRequest, context: ServerContext) async throws -> Cocoonbox_Control_V1_Share {
        try await world.setShareWrite(id: request.shareID, write: request.write)
    }

    func regenerate(request: Cocoonbox_Control_V1_ShareRef, context: ServerContext) async throws -> Cocoonbox_Control_V1_ShareSecret {
        try await world.regenerateShare(id: request.shareID)
    }

    func revoke(request: Cocoonbox_Control_V1_ShareRef, context: ServerContext) async throws -> Cocoonbox_Control_V1_RevokeShareResponse {
        try await world.revokeShare(id: request.shareID)
        return .init()
    }
}

struct SessionService: Cocoonbox_Agent_V1_Session.SimpleServiceProtocol {
    static let chunk = 64 << 10
    static let signInOrigin = "https://accounts.example.com"

    let world: MockWorld
    var credentials = true
    var workspace = FileManager.default.temporaryDirectory

    func hello(request: Cocoonbox_Agent_V1_HelloRequest, context: ServerContext) async throws -> Cocoonbox_Agent_V1_HelloResponse {
        .with {
            $0.agentVersion = "mockbox-0.1"
            $0.screenWidth = 1280
            $0.screenHeight = 800
            $0.models = ["deepseek/deepseek-v4.1-flash", "anthropic/claude-sonnet-5"]
            $0.defaultMainModel = "deepseek/deepseek-v4.1-flash"
        }
    }

    func activeOrigin(request: Cocoonbox_Agent_V1_ActiveOriginRequest, context: ServerContext) async throws -> Cocoonbox_Agent_V1_ActiveOriginResponse {
        guard credentials else { throw RPCError(code: .unimplemented, message: "") }
        return .with { $0.origin = Self.signInOrigin }
    }

    // fillCredential logs which fields a real box would fill, never their values.
    func fillCredential(request: Cocoonbox_Agent_V1_FillCredentialRequest, context: ServerContext) async throws -> Cocoonbox_Agent_V1_FillCredentialResponse {
        guard credentials else { throw RPCError(code: .unimplemented, message: "") }
        guard request.origin == Self.signInOrigin else {
            throw RPCError(code: .failedPrecondition, message: "the active tab is on \(Self.signInOrigin), not \(request.origin)")
        }
        var filled: [String] = []
        if !request.username.isEmpty, case .password = request.secret { filled.append("username") }
        switch request.secret {
        case .password: filled.append("password")
        case .code: filled.append("code")
        case nil: throw RPCError(code: .invalidArgument, message: "fill needs a password or a code")
        }
        print("fill origin=\(request.origin) filled=\(filled.joined(separator: ",")) submit=\(request.submit)")
        return .with {
            $0.filled = filled
            $0.submitted = request.submit
        }
    }

    func events(request: Cocoonbox_Agent_V1_EventsRequest, response: RPCWriter<Cocoonbox_Agent_V1_Event>, context: ServerContext) async throws {
        let (replay, live) = try await world.subscribe(from: request.fromOffset)
        for event in replay {
            try await response.write(event)
        }
        for await event in live {
            try await response.write(event)
        }
    }

    func screen(request: RPCAsyncSequence<Cocoonbox_Agent_V1_ScreenClientFrame, any Error>, response: RPCWriter<Cocoonbox_Agent_V1_ScreenServerFrame>, context: ServerContext) async throws {
        let (stream, continuation) = AsyncStream<Data>.makeStream()
        let server = SyntheticRFBServer { continuation.yield($0) }
        await world.attach(screen: server)
        defer {
            let text = server.editorText
            Task { await world.detach(screen: server, text: text) }
        }
        try await withThrowingTaskGroup(of: Void.self) { group in
            group.addTask {
                for await data in stream {
                    var offset = 0
                    while offset < data.count {
                        let end = min(offset + Self.chunk, data.count)
                        try await response.write(.with { $0.data = data.subdata(in: offset..<end) })
                        offset = end
                    }
                }
            }
            group.addTask {
                let deletions = await world.deletions
                while await world.deletions == deletions {
                    try await Task.sleep(for: .milliseconds(100))
                }
                throw RPCError(code: .unavailable, message: "box agent unreachable")
            }
            group.addTask {
                var opened = false
                for try await frame in request {
                    switch frame.body {
                    case .open:
                        opened = true
                    case .data(let data):
                        guard opened else {
                            throw RPCError(code: .failedPrecondition, message: "first Screen frame must be open")
                        }
                        server.receive(data)
                    case nil:
                        break
                    }
                }
                continuation.finish()
            }
            try await group.next()
            group.cancelAll()
        }
    }

    // exec runs the command with bash on this Mac in workspace, standing in for the box's desktop
    // user; it takes no terminal and ignores sizes.
    func exec(request: RPCAsyncSequence<Cocoonbox_Agent_V1_ExecClientFrame, any Error>, response: RPCWriter<Cocoonbox_Agent_V1_ExecServerFrame>, context: ServerContext) async throws {
        let (commands, command) = AsyncStream<String>.makeStream()
        let (signals, signal) = AsyncStream<Cocoonbox_Agent_V1_ExecSignal>.makeStream()
        let input = Pipe(), output = Pipe(), errors = Pipe()
        try await withThrowingTaskGroup(of: Void.self) { group in
            group.addTask {
                var started = false
                for try await frame in request {
                    switch frame.body {
                    case .start(let start) where !started:
                        started = true
                        print("exec tty=\(start.hasTty) term=\(start.tty.term) terminfo=\(start.tty.terminfo.utf8.count)")
                        command.yield(start.command)
                        command.finish()
                    case .stdin(let data) where started:
                        try? input.fileHandleForWriting.write(contentsOf: data)
                    case .eof where started:
                        try? input.fileHandleForWriting.close()
                    case .signal(let name) where started:
                        signal.yield(name)
                    case .resize where started:
                        break
                    default:
                        command.finish()
                        throw RPCError(code: .invalidArgument, message: "the first Exec frame must be start")
                    }
                }
                command.finish()
                try? input.fileHandleForWriting.close()
            }
            var commandLine: String?
            for await line in commands { commandLine = line }
            guard let commandLine else { return try await group.waitForAll() }
            let process = Process()
            process.executableURL = URL(fileURLWithPath: "/bin/bash")
            process.arguments = ["-c", commandLine]
            process.currentDirectoryURL = workspace
            process.standardInput = input
            process.standardOutput = output
            process.standardError = errors
            let (exits, exited) = AsyncStream<Int32>.makeStream()
            process.terminationHandler = { p in
                exited.yield(p.terminationReason == .uncaughtSignal ? 128 + p.terminationStatus : p.terminationStatus)
                exited.finish()
            }
            try process.run()
            group.addTask {
                for await name in signals {
                    if name == .int { process.interrupt() } else { process.terminate() }
                }
            }
            try await withTaskCancellationHandler {
                try await withThrowingTaskGroup(of: Void.self) { pipes in
                    pipes.addTask { for await data in Self.chunks(output.fileHandleForReading) { try await response.write(.with { $0.stdout = data }) } }
                    pipes.addTask { for await data in Self.chunks(errors.fileHandleForReading) { try await response.write(.with { $0.stderr = data }) } }
                    try await pipes.waitForAll()
                }
                for await code in exits {
                    try await response.write(.with { $0.exitCode = code })
                }
            } onCancel: {
                process.terminate()
            }
            group.cancelAll()
        }
    }

    private static func chunks(_ handle: FileHandle) -> AsyncStream<Data> {
        AsyncStream { continuation in
            handle.readabilityHandler = { handle in
                let data = handle.availableData
                if data.isEmpty {
                    handle.readabilityHandler = nil
                    continuation.finish()
                } else {
                    continuation.yield(data)
                }
            }
        }
    }
}
