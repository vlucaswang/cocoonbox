import CocoonBoxKit
import Foundation

// Plugin is the cocoonbox plugin as a local marketplace (design.md §13.5):
// manifests for Claude Code and Codex (Cursor reads the Claude one), one
// .mcp.json that runs the bridge, a skill and slash commands. No file holds
// a URL or a token; `command` is the path of the cocoonbox CLI.
public enum Plugin {
    public static let name = "cocoonbox"
    public static let marketplace = "cocoonbox"
    static let version = "0.16.0"
    static let summary = "Drive your cocoonbox, a cloud desktop or Android phone: its shell, files and screen, a desktop's browser, and its lifecycle (wake, save, upgrade, hibernate); move a coding task into it and back."

    // files maps paths under the marketplace root to their contents.
    public static func files(command: String) -> [String: String] {
        [
            ".claude-plugin/marketplace.json": json([
                "name": marketplace,
                "owner": ["name": "CMGS"],
                "metadata": ["description": summary],
                "plugins": [["name": name, "source": "./\(name)", "description": summary]],
            ]),
            ".agents/plugins/marketplace.json": json([
                "name": marketplace,
                "interface": ["displayName": "cocoonbox"],
                "plugins": [[
                    "name": name,
                    "source": ["source": "local", "path": "./\(name)"],
                    "policy": ["installation": "AVAILABLE", "authentication": "ON_INSTALL"],
                    "category": "Developer Tools",
                ]],
            ]),
            "\(name)/.claude-plugin/plugin.json": json(["name": name, "version": version, "description": summary, "author": ["name": "CMGS"]]),
            "\(name)/.codex-plugin/plugin.json": json([
                "name": name, "version": version, "description": summary,
                "mcpServers": [name: ["command": command, "args": ["mcp"], "required": true]], "skills": "./skills/",
                "interface": ["displayName": "cocoonbox", "shortDescription": summary, "category": "Developer Tools"],
            ]),
            "\(name)/.mcp.json": mcpConfig(command: command),
            "\(name)/skills/cocoonbox/SKILL.md": skill,
            "\(name)/skills/cocoonbox-offload/SKILL.md": offloadSkill(command: command),
            "\(name)/commands/status.md": slash("Show the state of my cocoonbox", "Call the cocoonbox tool box_status and tell me the state, the image, whether an upgrade is available and when my files were last saved."),
            "\(name)/commands/wake.md": slash("Wake my cocoonbox", "Call the cocoonbox tool box_ensure and tell me when the box is running."),
            "\(name)/commands/save.md": slash("Save my cocoonbox now", "Call the cocoonbox tool box_save. On a desktop box, tell me it closes the box's browser during the save. Report the result."),
            "\(name)/commands/sleep.md": slash("Hibernate my cocoonbox", "Call the cocoonbox tool box_hibernate and tell me when the box is hibernated."),
            "\(name)/commands/offload.md": slash("Move this coding task into my cocoonbox, where an agent keeps working on it", "Use the cocoonbox-offload skill: OFFLOAD this task to my box. $ARGUMENTS"),
            "\(name)/commands/back.md": slash("Bring the task back from my cocoonbox and continue it here", "Use the cocoonbox-offload skill: BACK. $ARGUMENTS"),
        ]
    }

    // mcpConfig is the stdio server entry every client runs.
    public static func mcpConfig(command: String) -> String {
        json(["mcpServers": [name: ["command": command, "args": ["mcp"]]]])
    }

    public static func write(to root: URL, command: String) throws {
        let manager = FileManager.default
        for (path, content) in files(command: command) {
            let file = root.appendingPathComponent(path)
            try manager.createDirectory(at: file.deletingLastPathComponent(), withIntermediateDirectories: true)
            try Data(content.utf8).write(to: file, options: .atomic)
        }
    }

    static let skill = """
    ---
    name: cocoonbox
    description: Work on the user's cocoonbox (a Linux desktop or an Android phone in the cloud, not this Mac) through the cocoonbox MCP tools. Use when the user asks for something on their box, their cloud desktop, their cloud phone or cocoonbox.
    ---

    The cocoonbox tools act on the user's cocoonbox, a cloud desktop or Android phone, not on this Mac. Local files are not the target.

    - Check `box_status` first when the state matters. A box tool wakes a hibernated box by itself; `box_ensure` wakes it on purpose.
    - The user may have several boxes. `box_list` lists them; every other tool takes `box`, a box name or id, and acts on the session's default box without it. Only a limited number of boxes run at once: hibernate one before waking another when a tool says so.
    - `box_create` adds a box. `box_delete` removes one and needs `box` and `confirm: true`, so ask the user first.
    - Use `shell`, `read_file`, `write_file`, `edit_file` and `list_dir` for files and commands on the box, the `browser_*` tools for its browser where the box has them, and `computer_state`, `computer_act` and `screenshot` for its screen. On an Android box, `shell` runs on the Ubuntu host with `adb` connected to the phone.
    - After a GUI action, take a `screenshot` to confirm it worked.
    - Call `box_save` before the user leaves for a long time. On a desktop box it closes the browser during the save.
    - `box_upgrade` restarts running programs and keeps files, logins and agent history; ask the user first, then call it with `confirm: true`.
    - Call `box_hibernate` when the user is done.
    - If `box_hibernate` or `box_ensure` times out, the operation continues on the box; call `box_status` and wait until the state settles.

    """

    // offloadSkill drives the local agent's side of an offload (design.md §27); `command` runs every mechanical step.
    static func offloadSkill(command: String) -> String {
        """
        ---
        name: cocoonbox-offload
        description: Hand this coding task to an agent in the user's cocoonbox (their box in the cloud) and take it back. Use when the user says offload, "move this to my box", "keep working while I'm away", "I'm back" or "bring it back", or when a message is exactly OFFLOAD or BACK.
        ---

        The agent in the box starts fresh. It knows only what your hand-over says, plus the repo.
        Write the hand-over as if for a capable engineer who has never seen this conversation.
        `cocoonbox` below is `\(command)`. Run it in the repo the user works in. It does every
        mechanical step: never copy files or secrets yourself, never print a secret, never edit the
        files it writes.

        ## OFFLOAD (on the Mac)

        1. Finish the step you are in, or stop at a safe point. Do not start new work.
        2. Run `cocoonbox offload handover-path` in the repo and write the hand-over to the path it
           prints, with the template below. Fill every section; write "none" for an empty one. Be
           concrete: exact commands, versions you saw, file paths, error messages, decisions with
           their reasons. Under "Secrets needed" give names only, never values.
        3. Run `cocoonbox offload --kind <claude|codex|cursor>`, naming the CLI you are unless the
           user asked for another one in the box. Add `--box NAME` when the user names a box.
           - Exit 3 is a block: stop and tell the user why, one line per finding. If the box CLI is
             missing or signed out, tell the user to open the CocoonBox app and use "Install in box"
             or "Sign in in the box" in the Offloaded section's box menu.
           - Exit 4 is warnings: tell the user each one, and run again with `--yes` only when they
             agree, or said beforehand to go ahead.
           - For a secret it warns about, ask the user; only they opt one in
             (`cocoonbox offload secrets add file:.env`). Never opt in on your own.
        4. When it prints `offloaded`, tell the user the box, that its agent runs with permission
           prompts bypassed, and that `/cocoonbox:back` or `cocoonbox back` returns the
           work. End your turn and make no more changes to this
           repo.
        5. On exit 1, report its last line. Nothing changed on this Mac; do not retry by hand.

        While the task is in the box, `cocoonbox offload inbox` shows what the box agent posted
        (progress, questions), `cocoonbox offload send "<answer>"` answers it, and
        `cocoonbox offload status` shows whether it is working. The app's Offloaded section shows
        the box agent's terminal.

        ## BACK (on the Mac)

        Run `cocoonbox back` in the repo. It waits for the box agent's hand-over, then lands the
        box's work against the snapshot taken at the offload. Report how the code landed:
        fast-forward, merged (applied uncommitted), unchanged, or a branch with conflicts, where
        your work tree is untouched. If it says nothing of this repo is in a box and names a
        `back.md`, the work has landed already: read that file as the hand-over. Then check the hand-over's claims against `git log` and `git diff`, run its
        verify commands, and continue from its next steps.

        ## Hand-over template

        ```markdown
        \(Handover.template)```

        """
    }

    // offloadPrompt is the OFFLOAD request a user pastes into a CLI without the offload skill, such as Codex.
    public static func offloadPrompt(command: String, handover: String, kind: OffloadKind) -> String {
        """
        OFFLOAD: move this task into my cocoonbox, where a fresh \(kind.title) session continues it while I am away. It knows only what your hand-over says, plus the repo.
        1. Finish the step you are in, or stop at a safe point. Do not start new work.
        2. Write a hand-over of this session to \(handover) with the template below. Fill every section; write "none" for an empty one. Be concrete: exact commands, versions, paths, errors, decisions with their reasons. Names only for secrets, never values. The file is outside this repo: if your sandbox refuses the write, ask me to approve it.
        3. Run in this repo: \(command) offload --kind \(kind.rawValue)
           It needs the network: if your sandbox refuses, ask me to approve it. Exit 3 is a block: tell me why, one line per finding. Exit 4 is warnings: tell me each one and run again with --yes only if I agree. Exit 1: tell me its last line.
        4. When it prints "offloaded", tell me the box, end your turn and change nothing more in this repo. `\(command) back` in this repo brings the work back.

        \(Handover.template)
        """
    }

    private static func slash(_ description: String, _ body: String) -> String {
        "---\ndescription: \(description)\n---\n\n\(body)\n"
    }

    private static func json(_ object: [String: Any]) -> String {
        let data = (try? JSONSerialization.data(withJSONObject: object, options: [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes])) ?? Data()
        return String(decoding: data, as: UTF8.self) + "\n"
    }
}
