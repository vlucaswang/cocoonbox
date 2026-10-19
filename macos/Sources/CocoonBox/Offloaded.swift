import AppKit
import CocoonBoxKit
import CocoonBoxTerminal
import SwiftUI

// OffloadedHeader titles the Offloaded section, with the box menu for each box's CLIs and the Claude Code plugin.
struct OffloadedHeader: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        HStack {
            Text("Offloaded")
            Spacer()
            Menu {
                Button("Install Claude Code Plugin…") { model.installClaudePlugin() }
                Divider()
                ForEach(model.boxes.filter { model.wording($0).browser }, id: \.id) { box in
                    Menu(box.name) {
                        ForEach(OffloadKind.allCases, id: \.self) { kind in
                            Button("Install \(kind.title) in Box…") { model.boxCLISheet = .install(kind, box) }
                        }
                        Divider()
                        ForEach(OffloadKind.allCases, id: \.self) { kind in
                            Button("Sign In to \(kind.title) in the Box…") { model.boxCLISheet = .signIn(kind, box) }
                        }
                    }
                }
            } label: {
                Image(systemName: "ellipsis.circle")
            }
            .menuStyle(.borderlessButton)
            .menuIndicator(.hidden)
            .fixedSize()
            .help("Install or sign in a box CLI, or install the Claude Code plugin for /cocoonbox:offload")
        }
    }
}

// OffloadRow is one offload: the repo, its box and CLI, the box CLI's status and the time since its last activity.
struct OffloadRow: View {
    @Environment(AppModel.self) private var model
    let record: OffloadRecord

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: "shippingbox")
                .foregroundStyle(Self.color(Self.status(model, record)))
            VStack(alignment: .leading, spacing: 2) {
                Text(Self.repoName(record)).lineLimit(1)
                Text(Self.place(model, record))
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                TimelineView(.periodic(from: .now, by: 30)) { context in
                    Text("\(Self.status(model, record)) · \(Self.since(model, record, now: context.date))")
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
            }
        }
    }

    static func repoName(_ record: OffloadRecord) -> String {
        record.top.map { URL(fileURLWithPath: $0).lastPathComponent } ?? record.name
    }

    static func place(_ model: AppModel, _ record: OffloadRecord) -> String {
        "\(model.boxes.first { $0.id == record.box }?.name ?? record.boxName) · \(record.kind.title)"
    }

    // status is the box CLI's status in words; a box that is not running cannot report one.
    static func status(_ model: AppModel, _ record: OffloadRecord) -> String {
        if let box = model.boxes.first(where: { $0.id == record.box }), box.state != .running { return "box \(StateBadge.label(box.state).lowercased())" }
        switch model.offloadActivity[record.repo]?.status {
        case "working": return "working"
        case "idle": return "idle"
        case "waiting_for_user": return "waiting for you"
        case "blocked": return "blocked"
        case "stopped": return "stopped"
        default: return "…"
        }
    }

    static func since(_ model: AppModel, _ record: OffloadRecord, now: Date) -> String {
        let at = model.offloadActivity[record.repo]?.at ?? Date(timeIntervalSince1970: Double(record.startedAtMs) / 1000)
        let minutes = Int(now.timeIntervalSince(at)) / 60
        switch minutes {
        case ..<1: return "just now"
        case ..<60: return "\(minutes) min ago"
        case ..<(24 * 60): return "\(minutes / 60) h ago"
        default: return at.formatted(date: .abbreviated, time: .shortened)
        }
    }

    static func color(_ status: String) -> Color {
        switch status {
        case "working": .green
        case "waiting for you": .orange
        case "blocked": .red
        default: .secondary
        }
    }
}

// OffloadPane shows the box CLI's terminal of an offload, with how to bring it back from the user's terminal.
struct OffloadPane: View {
    @Environment(AppModel.self) private var model
    let record: OffloadRecord

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 8) {
                Image(systemName: "shippingbox")
                Text(OffloadRow.repoName(record)).font(.headline).lineLimit(1)
                Text("\(OffloadRow.place(model, record)) · \(OffloadRow.status(model, record))")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .layoutPriority(-1)
                Spacer()
                if let box = model.boxes.first(where: { $0.id == record.box }) {
                    Menu("Box CLI") {
                        Button("Install \(record.kind.title) in \(box.name)…") { model.boxCLISheet = .install(record.kind, box) }
                        Button("Sign in in the box…") { model.boxCLISheet = .signIn(record.kind, box) }
                    }
                    .controlSize(.small)
                    .fixedSize()
                }
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
            Divider()
            HelpLine(record: record)
            Divider()
            if let terminal = model.boxTerminals[record.repo] {
                ZStack(alignment: .bottom) {
                    LocalAgentTerminal(session: terminal)
                        .id(ObjectIdentifier(terminal))
                    if terminal.exitCode != nil {
                        Button("Reattach") { model.reattach(record) }
                            .padding(12)
                            .help("The view of the box CLI ended; the box CLI may still run")
                    }
                }
                .frame(minHeight: 220, maxHeight: .infinity)
            } else {
                ProgressView("Attaching to the box CLI…")
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                    .task(id: record.repo) { model.openOffloadTerminal(record) }
            }
        }
        .task(id: record.repo) { await model.refreshOffloads() }
    }
}

// HelpLine tells the user to bring the offload back from their own terminal; the app only watches.
struct HelpLine: View {
    @Environment(AppModel.self) private var model
    let record: OffloadRecord

    var body: some View {
        let command = "/cocoonbox:back"
        HStack(spacing: 8) {
            Text("Bring this back from your terminal: \(command) (or cocoonbox back) in \(record.top ?? OffloadRow.repoName(record))")
                .font(.caption)
                .foregroundStyle(.secondary)
                .lineLimit(1)
                .truncationMode(.middle)
                .textSelection(.enabled)
            Spacer()
            Button("Copy") { model.copy(command) }
                .controlSize(.small)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
    }
}

// PluginSetupSheet shows the run of `cocoonbox setup claude`.
struct PluginSetupSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    let setup: PluginSetup

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Install the Claude Code plugin").font(.title2.bold())
            Text("This runs `cocoonbox setup claude`, which adds the cocoonbox plugin to your own Claude Code, or updates it. Then /cocoonbox:offload and /cocoonbox:back work in any repo. A running Claude Code loads it after /reload-plugins or a restart.")
                .font(.callout)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            ScrollView {
                Text(setup.output.isEmpty ? " " : setup.output)
                    .font(.caption.monospaced())
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .textSelection(.enabled)
            }
            .frame(height: 200)
            .background(.quaternary.opacity(0.4), in: RoundedRectangle(cornerRadius: 6))
            HStack {
                if let code = setup.code {
                    Label(code == 0 ? "Installed" : "Failed (exit \(code))", systemImage: code == 0 ? "checkmark.circle" : "xmark.octagon")
                        .foregroundStyle(code == 0 ? .green : .red)
                } else {
                    ProgressView().controlSize(.small)
                }
                Spacer()
                Button("Done") { dismiss() }
                    .keyboardShortcut(.defaultAction)
                    .disabled(setup.code == nil)
            }
        }
        .padding(20)
        .frame(width: 620)
    }
}
