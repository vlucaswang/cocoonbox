import Foundation

// MCPTarget is all this module knows about the box: the cocoonbox CLI whose
// stdio bridge serves it, the default box id, the endpoint, the bearer token
// and the app's socket. The token and the socket only ever reach a child
// through its environment, which the bridge reads.
public struct MCPTarget: Sendable {
    public let bridge: String
    public let box: String
    public let endpoint: String
    public let token: String
    public let socket: String?

    public init(bridge: String, box: String, endpoint: String, token: String, socket: String? = nil) {
        self.bridge = bridge
        self.box = box
        self.endpoint = endpoint
        self.token = token
        self.socket = socket
    }

    public var arguments: [String] {
        ["mcp", "--box", box, "--endpoint", endpoint]
    }
}

public enum LocalAgentKind: String, CaseIterable, Codable, Identifiable, Sendable {
    case claudeCode
    case codex
    case cursor

    public static let server = "cocoonbox"
    public static let tokenVariable = "COCOONBOX_TOKEN"
    public static let socketVariable = "COCOONBOX_APP_SOCKET"
    public static let agentVariable = "COCOONBOX_AGENT"
    public static let instructions = "Your computer is the user's cocoonbox, a cloud desktop or Android phone; use the cocoonbox MCP tools (shell, files, computer_*, and browser_* where the box has them) for all work on it; local files are not the target."

    public var id: String { rawValue }

    public var title: String {
        switch self {
        case .claudeCode: "Claude Code"
        case .codex: "Codex"
        case .cursor: "Cursor"
        }
    }

    public var executable: String {
        switch self {
        case .claudeCode: "claude"
        case .codex: "codex"
        case .cursor: "cursor-agent"
        }
    }

    // launch builds the child's argv, environment and workspace files: the CLI
    // runs the bridge as its stdio MCP server and hands it the token variable,
    // and with a socket the app's socket and agent's id. Only the environment
    // carries their values; argv and files name the variables. With resume the
    // CLI reopens `session`; Claude Code also starts a new conversation under `session`.
    public func launch(executable path: String, target: MCPTarget, agent: UUID? = nil, session: String? = nil, resume: Bool = false) -> LocalAgentLaunch {
        let server = Self.server
        var environment = [Self.tokenVariable: target.token]
        if let socket = target.socket, let agent {
            environment[Self.socketVariable] = socket
            environment[Self.agentVariable] = agent.uuidString
        }
        let variables = environment.keys.sorted()
        let arguments: [String]
        var files: [String: String] = [:]
        switch self {
        case .claudeCode:
            let config = Self.json(["mcpServers": [server: ["type": "stdio", "command": target.bridge, "args": target.arguments, "env": Dictionary(uniqueKeysWithValues: variables.map { ($0, "${\($0)}") })]]])
            arguments = [path, "--strict-mcp-config", "--mcp-config", config, "--append-system-prompt", Self.instructions, "--allowedTools", "mcp__\(server)"]
                + (session.map { resume ? ["--resume", $0] : ["--session-id", $0] } ?? [])
        case .codex:
            arguments = [path] + (resume ? session.map { ["resume", $0] } ?? [] : []) + [
                "-c", "mcp_servers.\(server).command=\(toml(target.bridge))",
                "-c", "mcp_servers.\(server).args=[\(target.arguments.map(toml).joined(separator: ", "))]",
                "-c", "mcp_servers.\(server).env_vars=[\(variables.map(toml).joined(separator: ", "))]",
                "-c", "mcp_servers.\(server).required=true",
                "-c", "developer_instructions=\(toml(Self.instructions))",
            ]
        case .cursor:
            arguments = [path, "--approve-mcps", "--trust"] + (resume ? session.map { ["--resume", $0] } ?? [] : [])
            files[".cursor/mcp.json"] = Self.json(["mcpServers": [server: ["command": target.bridge, "args": target.arguments, "env": Dictionary(uniqueKeysWithValues: variables.map { ($0, "${env:\($0)}") })]]])
            files[".cursor/cli.json"] = #"{"permissions":{"allow":["Mcp(*\#(server):*)"],"deny":[]}}"#
            files[".cursor/rules/cocoonbox.mdc"] = "---\ndescription: cocoonbox\nalwaysApply: true\n---\n\(Self.instructions)\n"
            files[".claude/settings.json"] = Self.json(["enabledPlugins": ["\(server)@\(server)": false]])
        }
        return LocalAgentLaunch(arguments: arguments, environment: environment, files: files)
    }

    // autoName takes the conversation's name from the CLI's terminal title,
    // without spinners, status and the workspace name; nil while the title
    // only names the CLI.
    public func autoName(fromTitle title: String) -> String? {
        let generic = Set([self.title, executable, rawValue, "Cursor Agent", "New Agent"].map { $0.lowercased() })
        for part in title.components(separatedBy: " | ") {
            let name = String(part.drop { !$0.isLetter && !$0.isNumber }).trimmingCharacters(in: .whitespaces)
            if !name.isEmpty, !generic.contains(name.lowercased()), !name.hasPrefix("Action Required") { return name }
        }
        return nil
    }

    private static func json(_ object: [String: Any]) -> String {
        let data = (try? JSONSerialization.data(withJSONObject: object, options: [.sortedKeys, .withoutEscapingSlashes])) ?? Data()
        return String(decoding: data, as: UTF8.self)
    }

    private func toml(_ value: String) -> String {
        "\"" + value.replacingOccurrences(of: "\\", with: "\\\\").replacingOccurrences(of: "\"", with: "\\\"") + "\""
    }
}

public struct LocalAgentLaunch: Sendable, Equatable {
    public var arguments: [String]
    public var environment: [String: String]
    public var files: [String: String]
}
