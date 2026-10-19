import CocoonBoxKit
import Foundation
import GRPCCore
import Synchronization
import Testing

// WatchBoxesTests follows Boxes.Watch through the Kit against MockBox: the watcher sees every change another client
// makes, and a resubscription starts again from the whole list.
@Suite struct WatchBoxesTests {
    @Test func watchSeesEveryChangeAnotherClientMakes() async throws {
        let mock = try await MockBoxProcess()
        defer { mock.stop() }
        let watcher = try BoxConnection(endpoint: mock.endpoint, token: "mock-token")
        defer { watcher.close() }
        let lists = Lists()
        let watch = Task {
            await watcher.watchBoxes(onBoxes: { list, _ in lists.append(list) }, onDisconnect: { _ in })
        }
        defer { watch.cancel() }
        #expect(await lists.seen(after: 0) { $0.count == 1 && $0[0].state == .hibernated })

        let main = try #require(try await mock.connection.firstBox())
        var from = lists.count
        _ = try await mock.connection.ensure(boxID: main.id)
        #expect(await lists.seen(after: from) { $0.first?.state == .running })
        from = lists.count
        _ = try await mock.connection.hibernate(boxID: main.id)
        #expect(await lists.seen(after: from) { $0.first?.state == .hibernated })
        from = lists.count
        _ = try await mock.connection.renameBox(boxID: main.id, name: "work")
        #expect(await lists.seen(after: from) { $0.first?.name == "work" })
        from = lists.count
        let created = try await mock.connection.createBox(name: "second").box.id
        #expect(await lists.seen(after: from) { $0.count == 2 })
        from = lists.count
        _ = try await mock.connection.deleteBox(boxID: created)
        #expect(await lists.seen(after: from) { $0.count == 1 && $0.first?.id == main.id })

        let all = lists.all
        #expect(zip(all, all.dropFirst()).allSatisfy { $0 != $1 }, "a list repeats the one before: \(all.map { $0.map(\.state) })")
    }

    @Test func watchCarriesTheOfferedImagesAndNewBoxOffersOneChoicePerFamily() async throws {
        let mock = try await MockBoxProcess()
        defer { mock.stop() }
        let watcher = try BoxConnection(endpoint: mock.endpoint, token: "mock-token")
        defer { watcher.close() }
        let offered = Mutex<[String]?>(nil)
        let watch = Task {
            await watcher.watchBoxes(onBoxes: { _, images in offered.withLock { $0 = images } }, onDisconnect: { _ in })
        }
        defer { watch.cancel() }
        #expect(await RecreateTests.until(seconds: 10) { offered.withLock { $0 } != nil })
        let choices = ImageChoice.choices(offered.withLock { $0 } ?? [])
        #expect(choices.map(\.label) == ["Desktop", "cbcore", "Android"])
        #expect(choices.map(\.image) == ["cbdesk", "cbcore", "cbdroid"])
        let phone = try await mock.connection.createBox(name: "phone", image: try #require(choices.last).image)
        #expect(phone.box.image == "cbdroid:v1")
        #expect(ImageChoice.choices(["cbcore:v25", "cbcore:v24"]).isEmpty)
    }

    @Test func aChoiceIsNamedByItsFamilyAfterTheLastSlashNotByItsPlace() {
        let android = ImageChoice.choices(["cbdroid:v3", "cbcore:v25"])
        #expect(android.map(\.label) == ["Android", "Desktop"] && android.map(\.image) == ["cbdroid", "cbcore"])
        let registry = ImageChoice.choices(["host:5000/cbcore:v1", "host:5000/cbdroid:v1"])
        #expect(registry.map(\.label) == ["Desktop", "Android"])
        #expect(registry.map(\.image) == ["host:5000/cbcore", "host:5000/cbdroid"])
        #expect(ImageChoice.choices(["host:5000/cbcore:v1", "host:5000/cbcore:v2"]).isEmpty)
    }

    @Test func aResubscriptionDeliversTheWholeListAgain() async {
        let calls = Mutex(0)
        let seen = Mutex<[String]>([])
        let follower = EventFollower<Cocoonbox_Control_V1_ListBoxesResponse>.boxes(backoff: Backoff(initial: .milliseconds(1), max: .milliseconds(1)))
        let list: @Sendable (Cocoonbox_Control_V1_BoxState) -> Cocoonbox_Control_V1_ListBoxesResponse = { state in
            Cocoonbox_Control_V1_ListBoxesResponse.with { $0.boxes = [.with { $0.id = "cb-1"; $0.state = state }] }
        }
        let task = Task {
            await follower.run(
                from: 0,
                subscribe: { offset, deliver in
                    let call = calls.withLock { c in
                        c += 1
                        return c
                    }
                    #expect(offset == 0)
                    switch call {
                    case 1:
                        await deliver(list(.running))
                        throw RPCError(code: .unavailable, message: "connection reset")
                    case 2:
                        await deliver(list(.running))
                        await deliver(list(.hibernated))
                        throw RPCError(code: .unavailable, message: "connection reset")
                    default:
                        throw CancellationError()
                    }
                },
                onEvent: { event in seen.withLock { $0.append("\(event.boxes[0].state)") } },
                onResync: {},
                onDisconnect: { _ in }
            )
        }
        #expect(await RecreateTests.until { calls.withLock { $0 } >= 3 })
        task.cancel()
        await task.value
        #expect(seen.withLock { $0 } == ["running", "running", "hibernated"])
    }
}

final class Lists: Sendable {
    private let state = Mutex<[[Cocoonbox_Control_V1_Box]]>([])

    var count: Int { state.withLock { $0.count } }

    var all: [[Cocoonbox_Control_V1_Box]] { state.withLock { $0 } }

    func append(_ list: [Cocoonbox_Control_V1_Box]) {
        state.withLock { $0.append(list) }
    }

    // seen waits for a list after the first `from` that matches.
    func seen(after from: Int, _ match: ([Cocoonbox_Control_V1_Box]) -> Bool) async -> Bool {
        await RecreateTests.until(seconds: 10) { state.withLock { $0.dropFirst(from).contains(where: match) } }
    }
}
