import Foundation

// ToolCatalog holds the tools the bridge lists without asking a box: the
// control tools from contracts.md "Control MCP", and the box tools from the
// cache of a box's last answer, else the contract's names with open schemas.
// Every box serves the same box tools, so they are listed once, with `box`.
public enum ToolCatalog {
    nonisolated(unsafe) static let box: [String: Any] = ["type": "string", "description": "The box's name or id, as box_list shows them; without it, the default box of this session."]

    nonisolated(unsafe) public static let control: [[String: Any]] = [
        tool("box_list", "List your cocoonbox boxes with their names, states and images.", [:]),
        tool("box_status", "Show the state of your cocoonbox box: running or hibernated, its image, whether an upgrade is available, and when your files were last saved.", ["box": box]),
        tool("box_ensure", "Start or wake your cocoonbox box and wait until it is running. Waking takes a few seconds; creating a new box takes longer.", ["box": box, "wait": ["type": "boolean", "default": true]]),
        tool("box_create", "Create another cocoonbox box with this name and wait until it is running. Your other boxes stay as they are; only a limited number may run at once.", ["name": ["type": "string"], "image": ["type": "string", "description": "An image family, such as cbdroid for an Android phone, or an image; without it, your usual desktop."], "wait": ["type": "boolean", "default": true]], required: ["name"]),
        tool("box_hibernate", "Hibernate your cocoonbox box. Running programs and memory are kept and come back on the next wake.", ["box": box]),
        tool("box_save", "Save your files, logins and agent history from the box to storage now. On a desktop box the browser closes during the save.", ["box": box]),
        tool("box_upgrade", "Move your box to the latest image of its kind. Running programs restart; files, logins and agent history are kept. Requires confirm: true.", ["box": box, "confirm": ["type": "boolean"]], required: ["confirm"]),
        tool("box_delete", "Delete one of your cocoonbox boxes. A running box is saved first; its saved state is kept for 7 days, then removed. Requires the box and confirm: true.", ["box": box, "confirm": ["type": "boolean"]], required: ["box", "confirm"]),
        tool("usage", "Show the model usage gw metered for your account, all your boxes together: requests, tokens and cost in micro-dollars today and this month (UTC), by model, with your daily cost cap (-1 unlimited) and what is left of it today. Your own model client's tokens are not in it.", [:]),
    ]

    public static let controlNames = Set(control.compactMap { $0["name"] as? String })

    // boxNames are the per-box tools of contracts.md "Remote tools over MCP".
    public static let boxNames = [
        "shell", "read_file", "write_file", "edit_file", "list_dir", "browser_navigate", "browser_snapshot",
        "browser_click", "browser_type", "browser_read", "computer_state", "computer_act", "screenshot",
    ]

    public static var boxFallback: [[String: Any]] {
        boxNames.filter { !$0.hasPrefix("browser_") }.map { ["name": $0, "description": "A tool on your cocoonbox. Its full schema loads once the box has run.", "inputSchema": ["type": "object"]] }
    }

    public static func loadBox(from file: URL) -> [[String: Any]]? {
        guard let data = try? Data(contentsOf: file), let tools = try? JSONSerialization.jsonObject(with: data) as? [[String: Any]], !tools.isEmpty else { return nil }
        return tools
    }

    public static func saveBox(_ tools: [[String: Any]], to file: URL) {
        try? FileManager.default.createDirectory(at: file.deletingLastPathComponent(), withIntermediateDirectories: true)
        if let data = try? JSONSerialization.data(withJSONObject: tools, options: [.sortedKeys]) {
            try? data.write(to: file, options: .atomic)
        }
    }

    // withBox adds the optional box argument to a box tool's input schema.
    static func withBox(_ tool: [String: Any]) -> [String: Any] {
        var tool = tool
        var schema = tool["inputSchema"] as? [String: Any] ?? ["type": "object"]
        var properties = schema["properties"] as? [String: Any] ?? [:]
        properties["box"] = box
        schema["properties"] = properties
        tool["inputSchema"] = schema
        return tool
    }

    static func same(_ a: [[String: Any]], _ b: [[String: Any]]) -> Bool {
        let encode = { (tools: [[String: Any]]) in try? JSONSerialization.data(withJSONObject: tools, options: [.sortedKeys]) }
        return encode(a) == encode(b)
    }

    private static func tool(_ name: String, _ description: String, _ properties: [String: Any], required: [String] = []) -> [String: Any] {
        ["name": name, "description": description, "inputSchema": ["type": "object", "properties": properties, "required": required]]
    }
}
