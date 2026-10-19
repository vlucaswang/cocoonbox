import CocoonBoxBridge
import CocoonBoxKit
import Darwin
import Foundation
import Synchronization

// cocoonbox drives the user's cocoonbox from a terminal and serves it to local
// agents over MCP (design.md §13). The token lives in the Keychain only.
let usage = """
usage: cocoonbox <command> [options]

  login [--endpoint URL] [--browser | --token - | --from-app]
                                        sign in: on a terminal (and with --browser) through the browser,
                                        where you allow this Mac on the console; the CLI gets a token of its
                                        own, revoked under Devices there. --token - reads a token you paste
                                        (also what a login over SSH or with stdin not a terminal does).
                                        The token goes into the Keychain
  login --invite CODE                   sign up through the browser with an invite code, and sign in
  invite create [--days N] [--json]     make a one-use invite link that lasts N days (default 2, at most 7)
  invite list [--json]                  list the invites your invite token made; both read an admin's
                                        invite token from COCOONBOX_INVITE_TOKEN, else from --token-file PATH
                                        or COCOONBOX_INVITE_TOKEN_FILE, never from the command line
  logout                                delete the stored token, and revoke it when the CLI got it through the
                                        browser (a token copied from the app stays the app's)
  logout --everywhere                   sign out every console session, the personal token, every device and
                                        every agent's access at once, this CLI too; boxes stay
  devices [--json]                      list the devices signed in through the browser; * marks this one
  devices revoke ID                     sign a device out at once, with every call it holds open
  status [--json]                       show the box
  ensure | save                         wake or save the box, with progress
  hibernate [--yes]                     hibernate the box, with progress
  upgrade [--yes]                       move the box to the latest image of its kind
  box create NAME [--image IMAGE] [--json]
                                        create another box and wait until it runs; IMAGE is an offered image
                                        or its family, such as cbdroid for an Android phone
  box schedule set BOX --cron "M H DOM MON DOW" --tz ZONE [--awake-for 15m] [--json]
                                        wake BOX from hibernation a minute before each time of the cron in
                                        ZONE, an IANA zone; after --awake-for (1m to 1h) it hibernates again
                                        unless a client reached it. Timers in the box run the routine.
  box schedule list [--json]            list the wake schedules of your boxes
  box schedule clear BOX                remove the wake schedule of BOX
  usage [--json]                        show the model usage gw metered for you today and this month (UTC),
                                        by model, with your daily cost cap and what is left of it
  flows [list] [--json]                 list your saved flows: your boxes sync them
  flows export [NAME...] [-o FILE]      write flows (default: all) as JSON to FILE, else stdout
  flows import FILE|- [--json]          add the flows of an export to your library; the last write wins
  flows rm NAME...                      delete flows from your library and from your boxes' unchanged copies
  keys add NAME --host HOST [--host HOST...] [--header H] [--template T | --query P] [--json]
                                        store an API key, read from stdin (or a prompt), as env var NAME in
                                        every box you own: its value there is a placeholder that the box's
                                        network swaps for the key on requests to HOST only (exact, or *.domain);
                                        it goes in Authorization as `Bearer {}` unless --header, --template
                                        (`{}`, `token {}`, `Basic USER:{}`, ...) or --query P say otherwise.
                                        A write share hands its guests the same use.
  keys ls [--json]                      list your keys: name, hosts, how each is sent, last 4 characters
  keys rotate NAME                      replace a key's value, read from stdin (or a prompt)
  keys rm NAME                          delete a key from every box; revoke it at the vendor too
  mcp [--no-wake]                       serve your boxes to a local agent over stdio MCP
  mcp-headers                           print the Authorization header as JSON (Claude Code headersHelper)
  rsh [--box NAME|ID] [-t] [HOST] COMMAND...
                                        run COMMAND in a box with its stdin, stdout and stderr; exits
                                        with its exit code, or 255 when the box cannot run it
  offload [--box NAME|ID] [--kind claude|codex|cursor] [--dir PATH] [--handover FILE] [--yes] [--json]
                                        move this repo's task into a box: preflight, copy, and a fresh box CLI
                                        session that starts from the hand-over
  offload --handover-prompt [--kind K]  print the request to paste into Codex (or another CLI without the
                                        plugin): it writes the hand-over, then runs offload
  offload handover-path                 print where to write the hand-over
  offload inbox [--wait SECONDS] [--all] [--json]
                                        print what the box agent posted since the last look
  offload status [--all] [--json]       show the box CLI session of this repo, or of every repo
  offload send [--interrupt] [TEXT...]  give the box agent a message (TEXT, else stdin)
  offload secrets [add|remove ENTRY]    list or change this repo's opted-in secrets: file:PATH, aws, kube,
                                        gcloud, ssh:KEY
  back [--interrupt] [--json]
                                        bring the task back: the box agent's hand-over, its commits and
                                        changes, merged against the offload's snapshot
  setup claude|codex|cursor [--print]   install the cocoonbox plugin or MCP entry for that client
  install                               link this binary into ~/.local/bin
  plugin export DIR                     write the plugin marketplace to DIR

--box NAME|ID picks the box of status, ensure, hibernate, save and upgrade (default: your first box)
and the default box of mcp (default: the newest running box).

rsh is the remote shell of rsync and git, which pass a host word first:
  rsync -az -e "cocoonbox rsh" ./ main:/workspace/repo/   (the host names the box)
  rsync -az -e "cocoonbox rsh --box main" ./ box:/workspace/repo/
  GIT_SSH_COMMAND="cocoonbox rsh" GIT_SSH_VARIANT=simple git clone main:/workspace/repo
Without --box, the first word names the box. With --box, which wins, a first word that is `box`
or names one of your boxes is dropped as the host. A user@ prefix and -l USER are ignored: the
command runs as the box's desktop user, by a login bash in /workspace. -t runs it on a terminal
when stdin is one, with this terminal's terminfo for a box that lacks it. The first SIGINT or
SIGTERM goes to the command; a second ends rsh, which kills it. rsh wakes a box that is not
running.

offload and back exit 0 when done, 1 on a failure (nothing changed on this Mac), 3 when the preflight
blocks, and 4 when it warns and --yes was not given: ask the user, then run again with --yes.
hibernate and upgrade ask for --yes while a box agent works on an offloaded task.

environment: COCOONBOX_ENDPOINT overrides the endpoint; COCOONBOX_CONFIG_DIR the settings directory;
BROWSER, a command, opens login's consent page in place of open(1);
COCOONBOX_TOKEN, which the app sets for the agents it starts, the token in the Keychain;
COCOONBOX_APP_SOCKET and COCOONBOX_AGENT, also set by the app, let mcp report its calls to the app
and take the default box the app sets.
"""

let arguments = Array(CommandLine.arguments.dropFirst())
let command = arguments.first ?? "help"
let flags = Set(arguments.dropFirst().filter { $0.hasPrefix("--") })
let valuedFlags: Set = ["--awake-for", "--box", "--cron", "--days", "--dir", "--endpoint", "--handover", "--image", "--invite", "--kind", "--token", "--token-file", "--tz", "--wait"]
let words = positional(arguments.dropFirst())

// rsh exits 255 on its own failures, so they differ from the command's exit codes.
let failureStatus: Int32 = command == "rsh" ? 255 : 1
let rshReport = command == "rsh" ? ProcessInfo.processInfo.environment["COCOONBOX_RSH_REPORT"].map { URL(fileURLWithPath: $0) } : nil
let rshStderr = Mutex(Data())

func fail(_ message: String) -> Never {
    FileHandle.standardError.write(Data("cocoonbox: \(message)\n".utf8))
    writeRshReport("failed: \(message)")
    exit(failureStatus)
}

func writeRshReport(_ status: String) {
    guard let rshReport else { return }
    try? (Data("\(status)\n".utf8) + rshStderr.withLock { $0 }).write(to: rshReport)
}

func value(of flag: String) -> String? {
    guard let index = arguments.firstIndex(of: flag), index + 1 < arguments.count else { return nil }
    return arguments[index + 1]
}

// MARK: settings

// home honours HOME, as the agent CLIs this one configures do.
let home = ProcessInfo.processInfo.environment["HOME"].map { URL(fileURLWithPath: $0) } ?? FileManager.default.homeDirectoryForCurrentUser
let support = ProcessInfo.processInfo.environment["COCOONBOX_CONFIG_DIR"].map { URL(fileURLWithPath: $0) }
    ?? home.appendingPathComponent("Library/Application Support/CocoonBox")
let settingsFile = support.appendingPathComponent("cli.json")
// Ad hoc builds of the CLI and the app are different code, so each keeps its own Keychain item.
let service = Keychain.service + ".cli"

func settings() -> [String: String] {
    guard let data = try? Data(contentsOf: settingsFile), let object = try? JSONSerialization.jsonObject(with: data) as? [String: String] else { return [:] }
    return object
}

func saveSettings(_ changes: [String: String?]) throws {
    var object = settings()
    for (key, value) in changes { object[key] = value }
    try FileManager.default.createDirectory(at: support, withIntermediateDirectories: true)
    try JSONSerialization.data(withJSONObject: object, options: [.withoutEscapingSlashes, .sortedKeys]).write(to: settingsFile, options: .atomic)
}

func savedEndpoint() -> String? {
    settings()["endpoint"]
}

// sourceKey names where the CLI's token for endpoint came from: browser (a device token of the CLI's own), paste or app.
// Only a browser token is the CLI's to revoke; one copied from the app is the app's device.
func sourceKey(_ endpoint: Endpoint) -> String {
    "token_source:" + endpoint.url
}

func endpoint(_ flag: String? = value(of: "--endpoint")) -> Endpoint {
    guard let url = flag ?? ProcessInfo.processInfo.environment["COCOONBOX_ENDPOINT"] ?? savedEndpoint() ?? Endpoint.configuredURL else {
        fail("no endpoint: pass --endpoint or set COCOONBOX_ENDPOINT")
    }
    guard let endpoint = Endpoint(url: url) else { fail("\(url) is not an http(s) URL") }
    guard endpoint.usesTLS || endpoint.isLoopback else { fail("\(endpoint.host) is not on this Mac, so it needs https: the token would travel in plaintext") }
    return endpoint
}

func token(_ endpoint: Endpoint) -> String {
    if let token = ProcessInfo.processInfo.environment["COCOONBOX_TOKEN"], !token.isEmpty { return token }
    if let token = Keychain.token(for: endpoint, service: service) ?? appToken(endpoint) { return token }
    let hint = ownPath.contains(".app/Contents/") ? "run cocoonbox login, or cocoonbox login --from-app once the CocoonBox app is signed in" : "run cocoonbox login"
    fail("not signed in to \(endpoint.url); \(hint)")
}

// appToken copies the app's token into the CLI's own item, which later runs read.
func appToken(_ endpoint: Endpoint) -> String? {
    guard let token = Keychain.token(for: endpoint), !token.isEmpty else { return nil }
    try? Keychain.setToken(token, for: endpoint, service: service)
    try? saveSettings([sourceKey(endpoint): "app"])
    FileHandle.standardError.write(Data("cocoonbox: using the CocoonBox app's sign-in\n".utf8))
    return token
}

func connection(_ endpoint: Endpoint = endpoint()) -> BoxConnection {
    do {
        return try BoxConnection(endpoint: endpoint, token: token(endpoint))
    } catch {
        fail("\(error)")
    }
}

// MARK: lifecycle

func stateName(_ state: Cocoonbox_Control_V1_BoxState) -> String {
    switch state {
    case .starting: "starting"
    case .running: "running"
    case .hibernating: "hibernating"
    case .hibernated: "hibernated"
    case .failed: "failed"
    default: "absent"
    }
}

// selectedBox is the box --box names, else the first box.
func selectedBox(_ connection: BoxConnection) async throws -> Cocoonbox_Control_V1_Box? {
    let boxes = try await connection.listBoxes()
    guard let ref = value(of: "--box") else { return boxes.first }
    return box(ref, in: boxes)
}

func box(_ ref: String, in boxes: [Cocoonbox_Control_V1_Box]) -> Cocoonbox_Control_V1_Box {
    guard let box = boxes.first(where: { $0.id == ref || $0.name == ref }) else { fail("you have no box \(ref)") }
    return box
}

func printBox(_ box: Cocoonbox_Control_V1_Box?, json: Bool) {
    let object: [String: Any] = [
        "box_id": box?.id ?? "", "name": box?.name ?? "", "state": stateName(box?.state ?? .unspecified), "image": box?.image ?? "",
        "latest_image": box?.latestImage ?? "", "last_saved_at_ms": box?.lastSavedAtMs ?? 0,
        "upgrade_available": box.map { !$0.latestImage.isEmpty && $0.latestImage != $0.image } ?? false,
    ]
    if json {
        let data = (try? JSONSerialization.data(withJSONObject: object, options: [.sortedKeys])) ?? Data()
        print(String(decoding: data, as: UTF8.self))
        return
    }
    guard let box else { return print("no box yet; run cocoonbox ensure") }
    var line = "\(box.name) \(box.id) \(stateName(box.state)) \(box.image)"
    if !box.latestImage.isEmpty, box.latestImage != box.image { line += " (upgrade available: \(box.latestImage))" }
    if box.lastSavedAtMs > 0 {
        line += ", saved \(Int(Date().timeIntervalSince1970 - Double(box.lastSavedAtMs) / 1000) / 60) min ago"
    }
    print(line)
}

// follow prints an operation's events to stderr until it ends and fails on PHASE_FAILED.
func follow(_ connection: BoxConnection, _ operation: String) async {
    let failed = Mutex<String?>(nil)
    do {
        try await connection.watchOperation(operation, after: "") { event in
            var line = "\(event.phase): \(event.detail)"
            if event.totalBytes > 0 { line += " (\(event.doneBytes.formatted(.byteCount(style: .file))) of \(event.totalBytes.formatted(.byteCount(style: .file))))" }
            FileHandle.standardError.write(Data((line + "\n").utf8))
            if event.phase == .failed { failed.withLock { $0 = event.detail } }
        }
    } catch {
        fail(BoxConnection.describe(error))
    }
    if let reason = failed.withLock({ $0 }) { fail(reason) }
}

func lifecycle(_ command: String) async {
    let connection = connection()
    defer { connection.close() }
    do {
        switch command {
        case "status":
            printBox(try await selectedBox(connection), json: flags.contains("--json"))
        case "ensure", "wake":
            let response = try await connection.ensure(boxID: try await selectedBox(connection)?.id ?? "")
            if response.hasOperation, !response.operation.id.isEmpty { await follow(connection, response.operation.id) }
            printBox(try await connection.listBoxes().first { $0.id == response.box.id }, json: flags.contains("--json"))
        case "hibernate", "save":
            guard let box = try await selectedBox(connection) else { fail("no box yet") }
            if command == "hibernate", !flags.contains("--yes"), let busy = await working(connection, box) {
                fail("\(busy); hibernating pauses it until the box wakes. Run again with --yes.")
            }
            let operation = command == "save" ? try await connection.saveState(boxID: box.id) : try await connection.hibernate(boxID: box.id)
            await follow(connection, operation.id)
            printBox(try await selectedBox(connection), json: flags.contains("--json"))
        case "upgrade":
            guard let box = try await selectedBox(connection) else { fail("no box yet") }
            guard !box.latestImage.isEmpty, box.latestImage != box.image else { return print("already on the latest image (\(box.image))") }
            guard flags.contains("--yes") else {
                let busy = await working(connection, box).map { " \($0); the upgrade restarts it from its last hand-over." } ?? ""
                fail("upgrading to \(box.latestImage) restarts running programs; files, logins and agent history are kept.\(busy) Run again with --yes.")
            }
            let operation = try await connection.rebase(boxID: box.id, image: box.latestImage)
            await follow(connection, operation.id)
            printBox(try await selectedBox(connection), json: flags.contains("--json"))
        default:
            break
        }
    } catch {
        fail(BoxConnection.describe(error))
    }
}

func boxCommand() async {
    if words.first == "schedule" { return await scheduleCommand() }
    guard words.first == "create", let name = words.dropFirst().first else {
        fail("usage: cocoonbox box create NAME [--image IMAGE]")
    }
    let image = value(of: "--image")
    if arguments.contains(where: { $0.hasPrefix("--image=") })
        || arguments.contains("--image") && (image?.isEmpty != false || image?.hasPrefix("--") == true) {
        fail("usage: cocoonbox box create NAME [--image IMAGE]")
    }
    let connection = connection()
    defer { connection.close() }
    do {
        let response = try await connection.createBox(name: name, image: image ?? "")
        if response.hasOperation, !response.operation.id.isEmpty { await follow(connection, response.operation.id) }
        printBox(try await connection.listBoxes().first { $0.id == response.box.id }, json: flags.contains("--json"))
    } catch {
        fail(BoxConnection.describe(error))
    }
}

// MARK: wake schedules

let scheduleUsage = "usage: cocoonbox box schedule [set BOX --cron \"M H DOM MON DOW\" --tz ZONE [--awake-for 15m] | list | clear BOX] [--json]"

// seconds reads a duration such as 90s, 15m or 1h.
func seconds(_ text: String) -> UInt32? {
    guard let unit = text.last, let factor = ["s": 1, "m": 60, "h": 3600][String(unit)], let count = UInt32(text.dropLast()) else { return nil }
    return count * UInt32(factor)
}

func scheduleObject(_ schedule: Cocoonbox_Control_V1_WakeSchedule, name: String) -> [String: Any] {
    ["box_id": schedule.boxID, "name": name, "cron": schedule.cron, "tz": schedule.tz, "awake_for_s": schedule.awakeForS,
     "next_at_ms": schedule.nextAtMs, "last_wake_at_ms": schedule.lastWakeAtMs]
}

func scheduleLine(_ schedule: Cocoonbox_Control_V1_WakeSchedule, name: String) -> String {
    let zone = TimeZone(identifier: schedule.tz) ?? .current
    func at(_ ms: Int64) -> String {
        Date(timeIntervalSince1970: Double(ms) / 1000).formatted(Date.ISO8601FormatStyle(timeZone: zone).year().month().day().time(includingFractionalSeconds: false).timeZone(separator: .omitted))
    }
    let awake = schedule.awakeForS % 60 == 0 ? "\(schedule.awakeForS / 60) min" : "\(schedule.awakeForS) s"
    var line = "\(name) \(schedule.boxID) \"\(schedule.cron)\" \(schedule.tz), awake \(awake), next \(at(schedule.nextAtMs))"
    if schedule.lastWakeAtMs > 0 { line += ", last wake \(at(schedule.lastWakeAtMs))" }
    return line
}

func scheduleCommand() async {
    let args = positional(after: "schedule")
    guard let action = args.first, ["set", "list", "clear"].contains(action), action == "list" || args.count == 2 else { fail(scheduleUsage) }
    let json = flags.contains("--json")
    let connection = connection()
    defer { connection.close() }
    do {
        let boxes = try await connection.listBoxes()
        let names = Dictionary(boxes.map { ($0.id, $0.name) }, uniquingKeysWith: { first, _ in first })
        switch action {
        case "set":
            let target = box(args[1], in: boxes)
            guard let cron = value(of: "--cron"), let tz = value(of: "--tz") else { fail(scheduleUsage) }
            var awakeFor: UInt32 = 0
            if let text = value(of: "--awake-for") {
                guard let parsed = seconds(text) else { fail("--awake-for is a duration such as 90s, 15m or 1h") }
                awakeFor = parsed
            }
            let schedule = try await connection.setWakeSchedule(boxID: target.id, cron: cron, tz: tz, awakeForSeconds: awakeFor)
            json ? printJSON(scheduleObject(schedule, name: target.name)) : print(scheduleLine(schedule, name: target.name))
        case "clear":
            let target = box(args[1], in: boxes)
            try await connection.clearWakeSchedule(boxID: target.id)
            json ? printJSON(["box_id": target.id, "name": target.name, "cleared": true]) : print("cleared the wake schedule of \(target.name)")
        default:
            let schedules = try await connection.listWakeSchedules()
            if json { return printJSON(["schedules": schedules.map { scheduleObject($0, name: names[$0.boxID] ?? "") }]) }
            guard !schedules.isEmpty else { return print("no wake schedules") }
            schedules.forEach { print(scheduleLine($0, name: names[$0.boxID] ?? "")) }
        }
    } catch {
        fail(BoxConnection.describe(error))
    }
}

// MARK: usage

func dollars(_ micros: Int64) -> String {
    "$" + String(format: "%.4f", Double(micros) / 1_000_000)
}

func periodLines(_ name: String, _ period: Cocoonbox_Control_V1_UsagePeriod) -> [String] {
    ["\(name): \(period.requests) requests, \(period.totalTokens) tokens, \(dollars(period.costMicros))"]
        + period.models.map { "  \($0.model): \($0.requests) requests, \($0.promptTokens) in, \($0.completionTokens) out, \(dollars($0.costMicros))" }
}

func usageCommand() async {
    let connection = connection()
    defer { connection.close() }
    do {
        let report = try await connection.usage()
        if flags.contains("--json") { return printJSON(report.object) }
        (periodLines("today (UTC)", report.today) + periodLines("this month", report.month)).forEach { print($0) }
        var cap = switch report.dailyCapMicros {
        case -1: "unlimited"
        case 0: "unknown"
        default: dollars(report.dailyCapMicros)
        }
        if report.defaultCap { cap += " (default)" }
        if report.remainingTodayMicros >= 0 { cap += ", \(dollars(report.remainingTodayMicros)) left today" }
        print("daily cap: \(cap)")
    } catch {
        fail(BoxConnection.describe(error))
    }
}

// MARK: flows

let flowsUsage = "usage: cocoonbox flows [list [--json] | export [NAME...] [-o FILE] | import FILE|- [--json] | rm NAME...]"

func flowObject(_ flow: Cocoonbox_Control_V1_Flow, content: Bool) -> [String: Any] {
    var object: [String: Any] = [
        "name": flow.name, "sha256": flow.sha256, "rev": flow.rev, "size": flow.size, "saved_at_ms": flow.savedAtMs,
        "box_id": flow.boxID, "image": flow.image,
    ]
    if content { object["content"] = String(decoding: flow.content, as: UTF8.self) }
    return object
}

func printJSON(_ object: Any) {
    let data = (try? JSONSerialization.data(withJSONObject: object, options: [.sortedKeys, .withoutEscapingSlashes])) ?? Data()
    print(String(decoding: data, as: UTF8.self))
}

// readExport turns an export file (or stdin for -) into flows to import: name, content and image of each.
func readExport(_ path: String) throws -> [Cocoonbox_Control_V1_Flow] {
    let data = path == "-" ? FileHandle.standardInput.readDataToEndOfFile() : try Data(contentsOf: URL(fileURLWithPath: path))
    guard let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any], object["version"] as? Int == 1,
          let items = object["flows"] as? [[String: Any]] else { fail("\(path) is no cocoonbox flows export (version 1)") }
    return items.enumerated().map { index, item in
        guard let name = item["name"] as? String, let content = item["content"] as? String else {
            fail("flow \(index) of \(path) has no name or content")
        }
        return .with {
            $0.name = name
            $0.content = Data(content.utf8)
            $0.image = item["image"] as? String ?? ""
        }
    }
}

func flowsCommand() async {
    let out = value(of: "-o")
    let verb = words.first ?? "list"
    let names = Array(words.dropFirst()).filter { $0 != "-o" && $0 != out }
    let connection = connection()
    defer { connection.close() }
    do {
        switch verb {
        case "list":
            let library = try await connection.listFlows()
            if flags.contains("--json") { return printJSON(["rev": library.rev, "flows": library.flows.map { flowObject($0, content: false) }]) }
            if library.flows.isEmpty { return print("no saved flows") }
            for flow in library.flows {
                let saved = Date(timeIntervalSince1970: Double(flow.savedAtMs) / 1000).formatted(.iso8601)
                let origin = flow.boxID.isEmpty ? "imported" : flow.boxID
                print("\(flow.name)  rev \(flow.rev)  \(flow.size) bytes  \(saved)  \(origin)  \(flow.image.isEmpty ? "-" : flow.image)")
            }
        case "export":
            let flows = try await connection.exportFlows(names: names)
            let object: [String: Any] = [
                "version": 1, "exported_at_ms": Int64(Date().timeIntervalSince1970 * 1000), "flows": flows.map { flowObject($0, content: true) },
            ]
            let data = try JSONSerialization.data(withJSONObject: object, options: [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes])
            guard let out else { return print(String(decoding: data, as: UTF8.self)) }
            try data.write(to: URL(fileURLWithPath: out), options: .atomic)
            FileHandle.standardError.write(Data("exported \(flows.count) flows to \(out)\n".utf8))
        case "import":
            guard let path = names.first else { fail(flowsUsage) }
            let stored = try await connection.importFlows(try readExport(path))
            if flags.contains("--json") { return printJSON(["flows": stored.map { flowObject($0, content: false) }]) }
            stored.forEach { print("imported \($0.name) as rev \($0.rev)") }
        case "rm":
            guard !names.isEmpty else { fail(flowsUsage) }
            let deleted = try await connection.deleteFlows(names: names)
            deleted.forEach { print("deleted \($0)") }
            let missing = names.filter { !deleted.contains($0) }
            if !missing.isEmpty { fail("your library has no \(missing.joined(separator: ", "))") }
        default:
            fail(flowsUsage)
        }
    } catch {
        fail(BoxConnection.describe(error))
    }
}

// MARK: keys

let keysUsage = "usage: cocoonbox keys [add NAME --host HOST... [--header H] [--template T | --query P] | ls | rotate NAME | rm NAME] [--json]"

// keyOptions splits the words after `keys` into positional words and the values of its flags; --host repeats and takes commas.
func keyOptions() -> (words: [String], values: [String: [String]]) {
    let valued: Set = ["--host", "--header", "--template", "--query", "--endpoint"]
    var words: [String] = []
    var values: [String: [String]] = [:]
    var rest = arguments.dropFirst().makeIterator()
    while let word = rest.next() {
        if valued.contains(word) {
            guard let value = rest.next() else { fail("\(word) needs a value") }
            values[word, default: []] += word == "--host" ? value.split(separator: ",").map(String.init) : [value]
        } else if !word.hasPrefix("--") {
            words.append(word)
        }
    }
    return (words, values)
}

// readKeyValue reads a key's value from stdin, or a prompt on a terminal, and never from the arguments.
func readKeyValue(_ name: String) -> String {
    var value: String
    if isatty(STDIN_FILENO) == 0 {
        value = String(decoding: FileHandle.standardInput.readDataToEndOfFile(), as: UTF8.self)
        if value.hasSuffix("\n") { value.removeLast() }
    } else {
        var buffer = [CChar](repeating: 0, count: 4096)
        guard let line = readpassphrase("value of \(name): ", &buffer, buffer.count, 0) else { fail("no value read") }
        value = String(cString: line)
    }
    guard !value.isEmpty else { fail("no value read for \(name)") }
    return value
}

func keyObject(_ key: Cocoonbox_Control_V1_Key) -> [String: Any] {
    [
        "name": key.name, "hosts": key.hosts, "header": key.header, "template": key.template, "query": key.query,
        "placeholder": key.placeholder, "hint": key.hint, "created_at_ms": key.createdAtMs, "updated_at_ms": key.updatedAtMs,
    ]
}

func keyLine(_ key: Cocoonbox_Control_V1_Key) -> String {
    let sent = key.query.isEmpty ? "\(key.header): \(key.template)" : "?\(key.query)={}"
    let created = Date(timeIntervalSince1970: Double(key.createdAtMs) / 1000).formatted(.iso8601)
    return "\(key.name)  \(key.hosts.joined(separator: ","))  \(sent)  \(key.hint.isEmpty ? "-" : "…" + key.hint)  \(created)"
}

// MARK: invite

// inviteToken reads the invite-scoped token from COCOONBOX_INVITE_TOKEN, else a file, so it never sits in argv.
func inviteToken() -> String {
    let environment = ProcessInfo.processInfo.environment
    if let token = environment["COCOONBOX_INVITE_TOKEN"], !token.isEmpty { return token }
    guard let path = value(of: "--token-file") ?? environment["COCOONBOX_INVITE_TOKEN_FILE"] else {
        fail("no invite token: set COCOONBOX_INVITE_TOKEN or pass --token-file PATH; an admin mints one on the console's admin page")
    }
    guard let data = FileManager.default.contents(atPath: path),
          let token = String(data: data, encoding: .utf8)?.trimmingCharacters(in: .whitespacesAndNewlines), !token.isEmpty
    else { fail("cannot read an invite token from \(path)") }
    return token
}

func inviteCall(_ endpoint: Endpoint, _ token: String, _ body: [String: Any]?) async throws -> (Any, Int) {
    var request = URLRequest(url: URL(string: endpoint.url + "/cb/api/invites")!)
    request.timeoutInterval = 15
    request.setValue("Bearer " + token, forHTTPHeaderField: "Authorization")
    if let body {
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try JSONSerialization.data(withJSONObject: body)
    }
    let session = URLSession(configuration: .ephemeral)
    defer { session.finishTasksAndInvalidate() }
    let (data, response) = try await session.data(for: request)
    return ((try? JSONSerialization.jsonObject(with: data)) ?? [:], (response as? HTTPURLResponse)?.statusCode ?? 0)
}

func inviteFailure(_ object: Any, _ status: Int) -> Never {
    fail((object as? [String: Any])?["error"] as? String ?? "the api answered HTTP \(status)")
}

func inviteLine(_ invite: [String: Any]) -> String {
    "\(invite["id"] as? String ?? "?")  uses left \(invite["uses_left"] as? Int ?? 0)  expires \(invite["expires_at"] as? String ?? "?")"
}

func inviteCommand() async {
    let endpoint = endpoint()
    let token = inviteToken()
    do {
        switch words.first ?? "list" {
        case "create":
            var body: [String: Any] = [:]
            if let days = value(of: "--days") {
                guard let n = Int(days) else { fail("--days takes a number of days, 1 to 7") }
                body["days"] = n
            }
            let (object, status) = try await inviteCall(endpoint, token, body)
            guard status == 200, let made = object as? [String: Any], let code = made["code"] as? String, let invite = made["invite"] as? [String: Any] else {
                inviteFailure(object, status)
            }
            let link = endpoint.url + "/cb/?invite=" + code
            if flags.contains("--json") { return printJSON(["link": link, "code": code, "invite": invite]) }
            print(link)
            print("one use, \(inviteLine(invite))")
        case "list", "ls":
            let (object, status) = try await inviteCall(endpoint, token, nil)
            guard status == 200, let invites = object as? [[String: Any]] else { inviteFailure(object, status) }
            if flags.contains("--json") { return printJSON(["invites": invites]) }
            if invites.isEmpty { return print("no invites") }
            invites.forEach { print(inviteLine($0)) }
        default:
            fail("usage: cocoonbox invite create [--days N] [--json] | invite list [--json]")
        }
    } catch {
        fail("\(error)")
    }
}

func keysCommand() async {
    let (words, values) = keyOptions()
    let verb = words.first ?? "ls"
    let name = words.dropFirst().first
    let connection = connection()
    defer { connection.close() }
    do {
        switch verb {
        case "ls", "list":
            let keys = try await connection.listKeys()
            if flags.contains("--json") { return printJSON(["keys": keys.map(keyObject)]) }
            if keys.isEmpty { return print("no keys") }
            keys.forEach { print(keyLine($0)) }
        case "add":
            guard let name, let hosts = values["--host"] else { fail(keysUsage) }
            let key = try await connection.addKey(.with {
                $0.name = name
                $0.hosts = hosts
                $0.header = values["--header"]?.last ?? ""
                $0.template = values["--template"]?.last ?? ""
                $0.query = values["--query"]?.last ?? ""
                $0.value = readKeyValue(name)
            })
            if flags.contains("--json") { return printJSON(keyObject(key)) }
            print("added \(keyLine(key))")
            print("new shells in your boxes see $\(key.name); a write share hands its guests the same use")
        case "rotate":
            guard let name else { fail(keysUsage) }
            let key = try await connection.rotateKey(name: name, value: readKeyValue(name))
            if flags.contains("--json") { return printJSON(keyObject(key)) }
            print("rotated \(keyLine(key))")
        case "rm", "delete":
            guard let name else { fail(keysUsage) }
            try await connection.deleteKey(name: name)
            print("deleted \(name) from every box; revoke it at the vendor too")
        default:
            fail(keysUsage)
        }
    } catch {
        fail(BoxConnection.describe(error))
    }
}

// MARK: devices

let devicesUsage = "usage: cocoonbox devices [list] [--json] | devices revoke ID"

func deviceObject(_ device: Cocoonbox_Control_V1_Device) -> [String: Any] {
    [
        "id": device.id, "client": device.client, "label": device.label, "created_at_ms": device.createdAtMs,
        "used_at_ms": device.usedAtMs, "current": device.current,
    ]
}

func deviceLine(_ device: Cocoonbox_Control_V1_Device) -> String {
    func at(_ ms: Int64) -> String { Date(timeIntervalSince1970: Double(ms) / 1000).formatted(.iso8601) }
    let used = device.usedAtMs > 0 ? "used \(at(device.usedAtMs))" : "never used"
    return "\(device.current ? "*" : " ") \(device.id)  \(device.client)  \(device.label.isEmpty ? "-" : device.label)  signed in \(at(device.createdAtMs))  \(used)"
}

func devicesCommand() async {
    let connection = connection()
    defer { connection.close() }
    do {
        switch words.first ?? "list" {
        case "list", "ls":
            let devices = try await connection.listDevices()
            if flags.contains("--json") { return printJSON(["devices": devices.map(deviceObject)]) }
            if devices.isEmpty { return print("no device signed in through the browser") }
            devices.forEach { print(deviceLine($0)) }
        case "revoke":
            guard words.count == 2 else { fail(devicesUsage) }
            try await connection.revokeDevice(id: words[1])
            print("revoked \(words[1])")
        default:
            fail(devicesUsage)
        }
    } catch {
        fail(BoxConnection.describe(error))
    }
}

// MARK: login

func readToken() -> String {
    if isatty(STDIN_FILENO) == 0 {
        return (readLine(strippingNewline: true) ?? "").trimmingCharacters(in: .whitespaces)
    }
    var buffer = [CChar](repeating: 0, count: 4096)
    guard let line = readpassphrase("cocoonbox token: ", &buffer, buffer.count, 0) else { return "" }
    return String(cString: line).trimmingCharacters(in: .whitespaces)
}

// pastesToken keeps today's paste for --token -, an SSH session and a stdin that is no terminal, unless --browser.
func pastesToken() -> Bool {
    if flags.contains("--invite") {
        guard !flags.contains("--token"), !flags.contains("--from-app") else { fail("--invite signs up through the browser; it takes neither --token nor --from-app") }
        return false
    }
    if flags.contains("--token") {
        guard value(of: "--token") == "-" else { fail("--token takes -: paste the token on stdin or at the prompt") }
        return true
    }
    if flags.contains("--browser") { return false }
    return isatty(STDIN_FILENO) == 0 || ProcessInfo.processInfo.environment["SSH_CONNECTION"] != nil
}

func browserToken(_ endpoint: Endpoint) async -> String {
    do {
        return try await BrowserLogin(endpoint: endpoint, client: BrowserLogin.cliClient, invite: value(of: "--invite") ?? "").run { url in
            FileHandle.standardError.write(Data("cocoonbox: allow this Mac in the browser; if no page opened, open\n  \(url)\n".utf8))
            openBrowser(url)
        }
    } catch {
        fail("\(error)")
    }
}

func openBrowser(_ url: URL) {
    let process = Process()
    if let browser = ProcessInfo.processInfo.environment["BROWSER"], !browser.isEmpty {
        process.executableURL = URL(fileURLWithPath: "/usr/bin/env")
        process.arguments = [browser, url.absoluteString]
    } else {
        process.executableURL = URL(fileURLWithPath: "/usr/bin/open")
        process.arguments = [url.absoluteString]
    }
    process.standardOutput = FileHandle.nullDevice
    do {
        try process.run()
    } catch {
        FileHandle.standardError.write(Data("cocoonbox: cannot open the browser: \(error)\n".utf8))
    }
}

func login() async {
    let endpoint = endpoint()
    let fromBrowser = !flags.contains("--from-app") && !pastesToken()
    let token: String
    if flags.contains("--from-app") {
        guard let app = Keychain.token(for: endpoint) else { fail("the app has no token for \(endpoint.url)") }
        token = app
    } else if fromBrowser {
        token = await browserToken(endpoint)
    } else {
        token = readToken()
    }
    guard !token.isEmpty else { fail("no token given") }
    let previous = Keychain.token(for: endpoint, service: service)
    let previousSource = settings()[sourceKey(endpoint)]
    do {
        let connection = try BoxConnection(endpoint: endpoint, token: token)
        defer { connection.close() }
        _ = try await connection.firstBox()
        try Keychain.setToken(token, for: endpoint, service: service)
        try saveSettings(["endpoint": endpoint.url, sourceKey(endpoint): flags.contains("--from-app") ? "app" : fromBrowser ? "browser" : "paste"])
    } catch {
        if fromBrowser { try? await BrowserLogin.revoke(token: token, endpoint: endpoint, client: BrowserLogin.cliClient) }
        fail(BoxConnection.describe(error))
    }
    // A replaced device token would otherwise stay live with nothing holding it.
    if let previous, previous != token, previousSource == "browser" {
        try? await BrowserLogin.revoke(token: previous, endpoint: endpoint, client: BrowserLogin.cliClient)
    }
    print("signed in to \(endpoint.url)")
}

func logout() async {
    let endpoint = endpoint()
    if flags.contains("--everywhere") {
        let connection = connection(endpoint)
        defer { connection.close() }
        do {
            try await connection.signOutEverywhere()
        } catch {
            fail(BoxConnection.describe(error))
        }
        Keychain.deleteToken(for: endpoint, service: service)
        try? saveSettings([sourceKey(endpoint): nil])
        return print("signed out everywhere")
    }
    if let token = Keychain.token(for: endpoint, service: service), settings()[sourceKey(endpoint)] == "browser" {
        do {
            try await BrowserLogin.revoke(token: token, endpoint: endpoint, client: BrowserLogin.cliClient)
        } catch {
            FileHandle.standardError.write(Data("cocoonbox: could not revoke the token at \(endpoint.url) (\(error)); revoke it under Devices on the console\n".utf8))
        }
    }
    Keychain.deleteToken(for: endpoint, service: service)
    try? saveSettings([sourceKey(endpoint): nil])
    print("signed out")
}

// MARK: mcp

// Output serialises writes from the bridge and its upstream streams to stdout.
final class Output: Sendable {
    private let lock = Mutex(())

    func write(_ data: Data) {
        lock.withLock { _ in FileHandle.standardOutput.write(data) }
    }
}

func serve() async {
    let endpoint = endpoint()
    let caches = ProcessInfo.processInfo.environment["COCOONBOX_CACHE_DIR"].map { URL(fileURLWithPath: $0) }
        ?? home.appendingPathComponent("Library/Caches/cocoonbox")
    let output = Output()
    let environment = ProcessInfo.processInfo.environment
    let link = environment["COCOONBOX_APP_SOCKET"].flatMap { path in environment["COCOONBOX_AGENT"].flatMap { AgentLinkClient(path: path, agent: $0) } }
    let bridge = Bridge(
        endpoint: endpoint, token: token(endpoint), wake: !flags.contains("--no-wake"), box: value(of: "--box"), link: link,
        cache: caches.appendingPathComponent("tools-\(endpoint.host).json"), output: { output.write($0) }
    )
    // a client that stops the bridge with SIGINT, SIGTERM or SIGHUP still gets its sessions closed; Claude Code sends SIGINT, then SIGKILL 0.5 s later
    let stopping = Mutex(false)
    let stops = [SIGINT, SIGTERM, SIGHUP].map { number in
        signal(number, SIG_IGN)
        let source = DispatchSource.makeSignalSource(signal: number, queue: .main)
        source.setEventHandler {
            if stopping.withLock({ again in defer { again = true }; return again }) { return }
            Task {
                await bridge.shutdown()
                exit(0)
            }
        }
        return source
    }
    stops.forEach { $0.resume() }
    defer { stops.forEach { $0.cancel() } }
    await bridge.run()
}

func headers() {
    let endpoint = endpoint()
    let data = (try? JSONSerialization.data(withJSONObject: ["Authorization": "Bearer \(token(endpoint))"])) ?? Data()
    print(String(decoding: data, as: UTF8.self))
}

// MARK: rsh

let rshUsage = "usage: cocoonbox rsh [--box NAME|ID] [-t] [-l USER] [HOST] COMMAND..."
let maxTerminfo = 256 << 10

// ExecInput is the frames an rsh call sends after start: stdin chunks read from fd 0 on their
// own thread, one read per frame the call asks for, so a chunk or two at most wait in memory,
// then eof; and any signal or terminal size sent in between.
struct ExecInput: AsyncSequence, Sendable {
    typealias Element = Cocoonbox_Agent_V1_ExecClientFrame
    typealias Failure = Never

    struct AsyncIterator: AsyncIteratorProtocol {
        var frames: AsyncStream<Element>.Iterator
        let wanted: DispatchSemaphore

        mutating func next() async -> Element? {
            wanted.signal()
            return await frames.next()
        }
    }

    private let frames: AsyncStream<Element>
    private let continuation: AsyncStream<Element>.Continuation
    private let wanted = DispatchSemaphore(value: 0)

    init() {
        (frames, continuation) = AsyncStream<Element>.makeStream()
        let (continuation, wanted) = (continuation, wanted)
        Thread.detachNewThread {
            var buffer = [UInt8](repeating: 0, count: 64 << 10)
            while true {
                wanted.wait()
                var n: Int
                repeat {
                    n = read(STDIN_FILENO, &buffer, buffer.count)
                } while n < 0 && retry(STDIN_FILENO, POLLIN)
                guard n > 0 else { break }
                let chunk = Data(buffer[..<n])
                continuation.yield(.with { $0.stdin = chunk })
            }
            continuation.yield(.with { $0.eof = Cocoonbox_Agent_V1_ExecEof() })
        }
    }

    func send(_ frame: Element) {
        continuation.yield(frame)
    }

    func makeAsyncIterator() -> AsyncIterator {
        AsyncIterator(frames: frames.makeAsyncIterator(), wanted: wanted)
    }
}

// Terminal puts the terminal on stdin in raw mode, as ssh -t does, and restores it.
final class Terminal: Sendable {
    private let saved: termios

    init?() {
        var saved = termios()
        guard isatty(STDIN_FILENO) != 0, tcgetattr(STDIN_FILENO, &saved) == 0 else { return nil }
        self.saved = saved
        var raw = saved
        cfmakeraw(&raw)
        tcsetattr(STDIN_FILENO, TCSANOW, &raw)
    }

    func restore() {
        var saved = saved
        tcsetattr(STDIN_FILENO, TCSANOW, &saved)
    }

    static var window: Cocoonbox_Agent_V1_ExecWindow {
        var size = winsize()
        _ = withUnsafeMutablePointer(to: &size) { ioctl(STDIN_FILENO, TIOCGWINSZ, $0) }
        return .with {
            $0.rows = UInt32(size.ws_row)
            $0.cols = UInt32(size.ws_col)
        }
    }
}

// terminfo is the local terminfo source of term as `infocmp -x` prints it, which a box without
// the entry compiles; empty when infocmp has none or it is larger than a box takes.
func terminfo(_ term: String) -> String {
    guard !term.isEmpty else { return "" }
    let process = Process()
    process.executableURL = URL(fileURLWithPath: "/usr/bin/env")
    process.arguments = ["infocmp", "-x", term]
    let output = Pipe()
    process.standardInput = FileHandle.nullDevice
    process.standardOutput = output
    process.standardError = FileHandle.nullDevice
    guard (try? process.run()) != nil else { return "" }
    let data = output.fileHandleForReading.readDataToEndOfFile()
    process.waitUntilExit()
    guard process.terminationStatus == 0, data.count <= maxTerminfo else { return "" }
    return String(decoding: data, as: UTF8.self)
}

// retry says whether a failed read or write of fd should run again, after waiting for fd when
// it is non-blocking: rsync hands its remote shell non-blocking descriptors.
func retry(_ fd: Int32, _ event: Int32) -> Bool {
    switch errno {
    case EINTR:
        return true
    case EAGAIN:
        var ready = pollfd(fd: fd, events: Int16(event), revents: 0)
        _ = poll(&ready, 1, -1)
        return true
    default:
        return false
    }
}

func writeAll(_ fd: Int32, _ data: Data) throws {
    try data.withUnsafeBytes { bytes in
        guard let base = bytes.baseAddress else { return }
        var done = 0
        while done < bytes.count {
            let n = write(fd, base + done, bytes.count - done)
            if n >= 0 {
                done += n
            } else if !retry(fd, POLLOUT) {
                throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO)
            }
        }
    }
}

func rsh() async {
    var rest = arguments.dropFirst()
    var options: [String: String] = [:]
    while let option = rest.first, option.hasPrefix("-") {
        if option == "--" {
            rest = rest.dropFirst()
            break
        }
        if option == "-t" {
            options[option] = ""
            rest = rest.dropFirst()
            continue
        }
        guard ["--box", "--endpoint", "-l"].contains(option), rest.count > 1 else { fail("unknown option \(option)\n\(rshUsage)") }
        options[option] = rest[rest.index(after: rest.startIndex)]
        rest = rest.dropFirst(2)
    }
    let connection = connection(endpoint(options["--endpoint"]))
    var words = Array(rest)
    do {
        let boxes = try await connection.listBoxes()
        func named(_ word: String) -> Cocoonbox_Control_V1_Box? {
            let host = word.split(separator: "@").last.map(String.init) ?? word
            return boxes.first { $0.id == host || $0.name == host }
        }
        var box: Cocoonbox_Control_V1_Box
        if let ref = options["--box"] {
            box = CocoonBoxCLI.box(ref, in: boxes)
            if let host = words.first, host.split(separator: "@").last == "box" || named(host) != nil { words.removeFirst() }
        } else {
            guard let host = words.first, let hosted = named(host) else { fail("name the box with --box or as the host\n\(rshUsage)") }
            box = hosted
            words.removeFirst()
        }
        guard !words.isEmpty else { fail("no command: rsh runs a command, not a login shell\n\(rshUsage)") }
        if box.state != .running {
            FileHandle.standardError.write(Data("cocoonbox: waking \(box.name)\n".utf8))
            let response = try await connection.ensure(boxID: box.id)
            if response.hasOperation, !response.operation.id.isEmpty { await follow(connection, response.operation.id) }
        }
        connection.boxID = box.id
    } catch {
        fail(BoxConnection.describe(error))
    }
    var start = Cocoonbox_Agent_V1_ExecStart.with { $0.command = words.joined(separator: " ") }
    let input = ExecInput()
    var terminal: Terminal?
    if options["-t"] != nil {
        if isatty(STDIN_FILENO) != 0 {
            let term = ProcessInfo.processInfo.environment["TERM"] ?? ""
            start.tty = .with {
                $0.term = term
                $0.window = Terminal.window
                $0.terminfo = terminfo(term)
            }
            terminal = Terminal()
        } else {
            FileHandle.standardError.write(Data("cocoonbox: no terminal for the command because stdin is not a terminal\n".utf8))
        }
    }
    let finish: @Sendable (Int32) -> Never = { [terminal] status in
        terminal?.restore()
        exit(status)
    }
    signal(SIGPIPE, SIG_IGN)
    let forwarded = Mutex(false)
    var sources = [(SIGINT, Cocoonbox_Agent_V1_ExecSignal.int), (SIGTERM, .term)].map { number, name in
        signal(number, SIG_IGN)
        let source = DispatchSource.makeSignalSource(signal: number, queue: .main)
        source.setEventHandler {
            if forwarded.withLock({ again in defer { again = true }; return again }) { finish(failureStatus) }
            input.send(.with { $0.signal = name })
        }
        return source
    }
    if terminal != nil {
        signal(SIGWINCH, SIG_IGN)
        let resize = DispatchSource.makeSignalSource(signal: SIGWINCH, queue: .main)
        resize.setEventHandler { input.send(.with { $0.resize = Terminal.window }) }
        sources.append(resize)
    }
    sources.forEach { $0.resume() }
    let code: Int32?
    do {
        code = try await connection.exec(start: start, input: input) { data, toStderr in
            if toStderr, rshReport != nil { rshStderr.withLock { $0.append(data) } }
            try writeAll(toStderr ? STDERR_FILENO : STDOUT_FILENO, data)
        }
    } catch {
        terminal?.restore()
        fail(BoxConnection.describe(error))
    }
    terminal?.restore()
    guard let code else { fail("the box ended the command without an exit code") }
    writeRshReport("exit \(code)")
    exit(code)
}

// MARK: offload

let offloadStore = OffloadStore(support: support)

// working names the box CLI sessions at work in a running box, for the warning before a hibernate or an upgrade.
func working(_ connection: BoxConnection, _ box: Cocoonbox_Control_V1_Box) async -> String? {
    guard box.state == .running else { return nil }
    let sessions = await offloader(connection, box).working()
    guard !sessions.isEmpty else { return nil }
    return "a box agent is working on " + sessions.map { "\($0.name) (\($0.cli))" }.joined(separator: ", ")
}

func offloader(_ connection: BoxConnection, _ box: Cocoonbox_Control_V1_Box) -> Offloader {
    Offloader(connection: connection, box: box, token: token(connection.endpoint), cli: executable, store: offloadStore,
              environment: ProcessInfo.processInfo.environment) { FileHandle.standardError.write(Data("\($0)\n".utf8)) }
}

// runningBox is the box ref names, else the first box, woken when it does not run.
func runningBox(_ connection: BoxConnection, _ ref: String?) async throws -> Cocoonbox_Control_V1_Box {
    let boxes = try await connection.listBoxes()
    guard let box = ref.map({ CocoonBoxCLI.box($0, in: boxes) }) ?? boxes.first else { fail("no box yet") }
    guard box.state != .running else { return box }
    FileHandle.standardError.write(Data("cocoonbox: waking \(box.name)\n".utf8))
    let response = try await connection.ensure(boxID: box.id)
    if response.hasOperation, !response.operation.id.isEmpty { await follow(connection, response.operation.id) }
    return try await connection.listBoxes().first { $0.id == box.id } ?? box
}

// offloadKey is the repo key of --dir or the working directory. For an offload in flight it falls back
// to the record of the same top level, whose key changes when an empty repo gets its first commit.
func offloadKey(inFlight: Bool = false) -> String {
    let dir = URL(fileURLWithPath: value(of: "--dir") ?? FileManager.default.currentDirectoryPath)
    guard let repo = GitRepo.open(dir, environment: ProcessInfo.processInfo.environment) else { fail("\(dir.path) is not in a git repository") }
    guard inFlight, offloadStore.record(repo.key) == nil else { return repo.key }
    return offloadStore.records().last { $0.top == repo.top.path }?.repo ?? repo.key
}

// positional is the words after sub that are neither options nor their values.
func positional(after sub: String) -> [String] {
    positional(arguments.drop { $0 != sub }.dropFirst())
}

func positional(_ arguments: ArraySlice<String>) -> [String] {
    var words: [String] = []
    var rest = arguments
    while let word = rest.first {
        rest = rest.dropFirst()
        if valuedFlags.contains(word) {
            rest = rest.dropFirst()
        } else if !word.hasPrefix("--") {
            words.append(word)
        }
    }
    return words
}
func findingObjects(_ findings: [Finding]) -> [[String: String]] {
    findings.map { ["check": $0.check, "outcome": $0.outcome.rawValue, "detail": $0.detail] }
}

func printFindings(_ findings: [Finding]) {
    for finding in findings {
        print("\(finding.outcome.rawValue.padding(toLength: 8, withPad: " ", startingAt: 0))\(finding.check): \(finding.detail)")
    }
}

func offload() async {
    let sub = arguments.dropFirst().first.flatMap { ["handover-path", "inbox", "status", "send", "secrets"].contains($0) ? $0 : nil }
    if sub == "handover-path" {
        do {
            return print(try offloadStore.handover(offloadKey()).path)
        } catch {
            fail("\(error)")
        }
    }
    if sub == "secrets" { return secrets() }
    if flags.contains("--handover-prompt") {
        let kind = value(of: "--kind").flatMap(OffloadKind.init) ?? OffloadKind.current(ProcessInfo.processInfo.environment) ?? .codex
        do {
            return print(Plugin.offloadPrompt(command: executable, handover: try offloadStore.handover(offloadKey()).path, kind: kind))
        } catch {
            fail("\(error)")
        }
    }
    let connection = connection()
    defer { connection.close() }
    let json = flags.contains("--json")
    do {
        switch sub {
        case "inbox", "status", "send":
            let all = flags.contains("--all")
            let key = all && sub == "status" ? nil : offloadKey(inFlight: true)
            let record = key.flatMap(offloadStore.record)
            let packer = offloader(connection, try await runningBox(connection, value(of: "--box") ?? record?.box))
            if sub == "status" {
                let sessions = try await packer.status(key)
                if json { return printJSON(["sessions": sessions.map { ["repo": $0.repo, "name": $0.name, "cli": $0.cli, "offload_id": $0.offloadID, "status": $0.status, "hooks_silent": $0.hooksSilent] }]) }
                if sessions.isEmpty { return print("no box CLI session") }
                return sessions.forEach { print("\($0.name) \($0.cli) \($0.status) offload \($0.offloadID) (repo \($0.repo))" + ($0.hooksSilent ? "; its hooks do not run, so the status comes from its transcript" : "")) }
            }
            guard let key else { fail("name the repo") }
            if sub == "send" {
                let rest = positional(after: "send")
                let text = rest.isEmpty ? String(decoding: FileHandle.standardInput.readDataToEndOfFile(), as: UTF8.self) : rest.joined(separator: " ")
                guard !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { fail("no message") }
                let delivery = try await packer.send(key, message: text, interrupt: flags.contains("--interrupt"))
                return json ? printJSON(["delivery": delivery]) : print("sent to the box agent: \(delivery)")
            }
            let since = all ? 0 : record?.seen ?? 0
            let wait = value(of: "--wait").flatMap { Int($0) }
            var (entries, status) = try await packer.read(key, since: since, wait: wait)
            var newest = entries.last?.seq ?? since
            if !all, let id = record?.id {
                let current = { (entry: OutboxEntry) in entry.offloadID == id || entry.offloadID.isEmpty }
                if !entries.isEmpty, !entries.contains(where: current) {
                    (entries, status) = try await packer.read(key, since: newest, wait: wait)
                    newest = entries.last?.seq ?? newest
                }
                entries = entries.filter(current)
            }
            if var record, newest > record.seen {
                record.seen = newest
                try? offloadStore.save(record)
            }
            if json {
                return printJSON(["status": status, "entries": entries.map { ["seq": $0.seq, "kind": $0.kind, "offload_id": $0.offloadID, "cli": $0.cli, "time_ms": $0.timeMs, "body": $0.body] }])
            }
            if entries.isEmpty { print("nothing new from the box agent (\(status.isEmpty ? "no session" : status))") }
            for entry in entries {
                let time = Date(timeIntervalSince1970: Double(entry.timeMs) / 1000).formatted(date: .omitted, time: .shortened)
                print("#\(entry.seq) \(entry.kind) at \(time)\n\(entry.body)\n")
            }
        default:
            if let named = value(of: "--kind"), OffloadKind(rawValue: named) == nil { fail("--kind is claude, codex or cursor") }
            guard let kind = value(of: "--kind").flatMap(OffloadKind.init) ?? OffloadKind.current(ProcessInfo.processInfo.environment) else {
                fail("name the box CLI with --kind claude, codex or cursor")
            }
            let key = offloadKey()
            let file = try value(of: "--handover").map { URL(fileURLWithPath: $0) } ?? offloadStore.handover(key)
            guard let handover = try? String(contentsOf: file, encoding: .utf8) else { fail("no hand-over at \(file.path); write it there first (cocoonbox offload handover-path)") }
            let box = try await runningBox(connection, value(of: "--box"))
            let request = OffloadRequest(directory: URL(fileURLWithPath: value(of: "--dir") ?? FileManager.default.currentDirectoryPath),
                                         kind: kind, handover: handover, yes: flags.contains("--yes"))
            let outcome = try await offloader(connection, box).offload(request)
            let record = outcome.record
            if json {
                return printJSON(["ok": true, "box": box.name, "box_id": box.id, "dir": record.dir, "tmux": record.tmux, "offload_id": record.id, "repo": record.repo,
                                  "accepted": outcome.accepted, "sent_bytes": outcome.sent, "findings": findingObjects(outcome.findings),
                                  "timings_ms": Dictionary(uniqueKeysWithValues: outcome.timings.map { ($0.0, Int($0.1 * 1000)) })])
            }
            printFindings(outcome.findings)
            print("offloaded to \(box.name): \(outcome.cli)")
            print("  copy \(record.dir), tmux \(record.tmux), offload \(record.id)")
            print("  sent \(outcome.sent.formatted(.byteCount(style: .file))); " + outcome.timings.map { "\($0.0) \(String(format: "%.1f", $0.1)) s" }.joined(separator: ", "))
            if !outcome.accepted { print("  the box CLI has not taken the hand-over yet; watch it with `cocoonbox offload status`") }
            print("Bring it back with /cocoonbox:back or `cocoonbox back`.")
        }
    } catch let error as OffloadError {
        offloadFailed(error, json: json)
    } catch {
        fail(BoxConnection.describe(error))
    }
}

func offloadFailed(_ error: OffloadError, json: Bool) -> Never {
    let code: Int32 = switch error {
    case .refused: 3
    case .needsYes: 4
    default: 1
    }
    if json {
        printJSON(["ok": false, "code": code == 3 ? "refused" : code == 4 ? "confirm" : "failed", "error": error.description, "findings": findingObjects(error.findings)])
    } else {
        printFindings(error.findings)
        if error.findings.contains(where: { $0.outcome == .block && $0.check.contains("cli") }) {
            print("Open the CocoonBox app: \"Install in box\" and \"Sign in in the box\" are in the Offloaded section's box menu. Or: cocoonbox rsh --box BOX -t '<the CLI's login>'")
        }
        FileHandle.standardError.write(Data("cocoonbox: \(error)\(code == 1 ? "; nothing changed on this Mac" : "")\n".utf8))
    }
    exit(code)
}

func back() async {
    let connection = connection()
    defer { connection.close() }
    let json = flags.contains("--json")
    let key = offloadKey(inFlight: true)
    guard let record = offloadStore.record(key) else {
        let last = offloadStore.root.appendingPathComponent("\(key)/back.md")
        let modified = (try? FileManager.default.attributesOfItem(atPath: last.path)[.modificationDate]) as? Date
        fail("nothing of this repo is in a box" + (modified.map { "; the hand-over brought back at \($0.formatted(date: .abbreviated, time: .shortened)) is in \(last.path)" } ?? ""))
    }
    do {
        let packer = offloader(connection, try await runningBox(connection, record.box))
        let outcome = try await packer.back(key, interrupt: flags.contains("--interrupt")) { entry in
            FileHandle.standardError.write(Data("box agent \(entry.kind) #\(entry.seq)\(entry.kind == "question" ? ":\n" + entry.body : "")\n".utf8))
        }
        let landing = outcome.landing?.summary ?? "no repo"
        if json { return printJSON(["ok": true, "landing": landing, "handover": outcome.handover, "file": outcome.file.path, "offload_id": record.id]) }
        print(landing)
        print("The box agent's hand-over (also in \(outcome.file.path)):\n")
        print(outcome.handover)
    } catch let error as OffloadError {
        offloadFailed(error, json: json)
    } catch {
        fail(BoxConnection.describe(error))
    }
}

// secrets lists or changes the repo's opted-in secrets; only names and paths are stored.
func secrets() {
    let key = offloadKey()
    var entries = offloadStore.secrets(key)
    let rest = positional(after: "secrets")
    if let action = rest.first {
        guard ["add", "remove"].contains(action), rest.count == 2 else { fail("usage: cocoonbox offload secrets [add|remove ENTRY]") }
        let entry = rest[1]
        guard entry.hasPrefix("file:") || entry.hasPrefix("ssh:") || ["aws", "kube", "gcloud"].contains(entry) else { fail("an entry is file:PATH, aws, kube, gcloud or ssh:KEY") }
        entries = action == "add" ? entries.filter { $0 != entry } + [entry] : entries.filter { $0 != entry }
        do {
            try offloadStore.setSecrets(entries, for: key)
        } catch {
            fail("\(error)")
        }
    }
    print(entries.isEmpty ? "no secrets opted in" : entries.joined(separator: "\n"))
}

// MARK: setup

let ownPath = (Bundle.main.executableURL ?? URL(fileURLWithPath: CommandLine.arguments[0])).resolvingSymlinksInPath().path

let executable: String = {
    let linked = home.appendingPathComponent(".local/bin/cocoonbox").path
    let target = (try? FileManager.default.destinationOfSymbolicLink(atPath: linked)).map { URL(fileURLWithPath: $0).resolvingSymlinksInPath().path }
    return target == ownPath ? linked : ownPath
}()

func run(_ argv: [String]) -> Bool {
    FileHandle.standardError.write(Data(("$ " + argv.joined(separator: " ") + "\n").utf8))
    let process = Process()
    process.executableURL = URL(fileURLWithPath: "/usr/bin/env")
    process.arguments = argv
    do {
        try process.run()
        process.waitUntilExit()
    } catch {
        return false
    }
    return process.terminationStatus == 0
}

func setup() {
    guard let client = words.first, ["claude", "codex", "cursor"].contains(client) else { fail("setup needs claude, codex or cursor") }
    let root = support.appendingPathComponent("plugins")
    if client == "cursor" {
        if flags.contains("--print") { return print("Merge into ~/.cursor/mcp.json:\n\(Plugin.mcpConfig(command: executable))") }
        let file = home.appendingPathComponent(".cursor/mcp.json")
        // an existing file is merged into; one this cannot parse is left alone
        var config: [String: Any] = [:]
        if let data = try? Data(contentsOf: file), !data.isEmpty {
            guard let parsed = try? JSONSerialization.jsonObject(with: data) as? [String: Any], parsed["mcpServers"] == nil || parsed["mcpServers"] is [String: Any] else {
                fail("\(file.path) is not plain JSON with an mcpServers object; left unchanged. Add this entry by hand:\n\(Plugin.mcpConfig(command: executable))")
            }
            config = parsed
        }
        var servers = config["mcpServers"] as? [String: Any] ?? [:]
        servers[Plugin.name] = ["command": executable, "args": ["mcp"]]
        config["mcpServers"] = servers
        do {
            try FileManager.default.createDirectory(at: file.deletingLastPathComponent(), withIntermediateDirectories: true)
            try JSONSerialization.data(withJSONObject: config, options: [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes]).write(to: file, options: .atomic)
        } catch {
            fail("\(error)")
        }
        return print("added cocoonbox to \(file.path)")
    }
    do {
        try? FileManager.default.removeItem(at: root.appendingPathComponent(Plugin.name))
        try Plugin.write(to: root, command: executable)
    } catch {
        fail("\(error)")
    }
    let id = "\(Plugin.name)@\(Plugin.marketplace)"
    // install leaves an installed plugin at its recorded version, so update moves it to this one
    let steps: [[String]] = client == "claude"
        ? [["claude", "plugin", "marketplace", "add", root.path], ["claude", "plugin", "install", id], ["claude", "plugin", "update", id]]
        : [["codex", "plugin", "marketplace", "add", root.path], ["codex", "plugin", "add", id]]
    if flags.contains("--print") { return steps.forEach { print($0.joined(separator: " ")) } }
    let update = client == "claude" ? ["claude", "plugin", "marketplace", "update", Plugin.marketplace] : ["codex", "plugin", "marketplace", "upgrade", Plugin.marketplace]
    guard run(steps[0]) || run(update), steps.dropFirst().allSatisfy(run) else { fail("\(client) did not install the plugin") }
    print("installed the cocoonbox plugin for \(client)")
    let target = endpoint()
    if Keychain.token(for: target, service: service) == nil { _ = appToken(target) }
}

func install() {
    let bin = home.appendingPathComponent(".local/bin")
    let link = bin.appendingPathComponent("cocoonbox")
    do {
        try FileManager.default.createDirectory(at: bin, withIntermediateDirectories: true)
        try? FileManager.default.removeItem(at: link)
        try FileManager.default.createSymbolicLink(atPath: link.path, withDestinationPath: ownPath)
    } catch {
        fail("\(error)")
    }
    print("linked \(link.path) -> \(ownPath)")
}

// MARK: main

switch command {
case "login":
    await login()
case "logout":
    await logout()
case "status", "ensure", "wake", "hibernate", "save", "upgrade":
    await lifecycle(command)
case "box":
    await boxCommand()
case "usage":
    await usageCommand()
case "flows":
    await flowsCommand()
case "keys":
    await keysCommand()
case "devices":
    await devicesCommand()
case "invite":
    await inviteCommand()
case "mcp":
    await serve()
case "mcp-headers":
    headers()
case "rsh":
    await rsh()
case "offload":
    await offload()
case "back":
    await back()
case "setup":
    setup()
case "install":
    install()
case "plugin":
    guard words.first == "export", words.count > 1 else { fail("usage: cocoonbox plugin export DIR") }
    do {
        try Plugin.write(to: URL(fileURLWithPath: words[1]), command: executable)
    } catch {
        fail("\(error)")
    }
default:
    print(usage)
}
