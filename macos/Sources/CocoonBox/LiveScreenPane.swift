import CocoonBoxKit
import SwiftUI

struct LiveScreenView: NSViewRepresentable {
    let container: LiveScreenContainer

    func makeNSView(context: Context) -> LiveScreenContainer {
        container.removeFromSuperview()
        return container
    }

    func updateNSView(_ nsView: LiveScreenContainer, context: Context) {}
}

struct LiveScreenPane: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        VStack(spacing: 0) {
            ScreenStatusBar()
            ZStack {
                LiveScreenView(container: model.screen.container)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                if model.screen.status != .live {
                    Color.black.opacity(0.6)
                        .allowsHitTesting(false)
                    VStack(spacing: 8) {
                        if model.screen.status == .connecting || working { ProgressView() }
                        Text(message).foregroundStyle(.white.opacity(0.85))
                    }
                }
            }
            .overlay(alignment: .topLeading) {
                if model.screen.takeOver {
                    Text("You are in control")
                        .font(.caption.bold())
                        .padding(.horizontal, 8)
                        .padding(.vertical, 3)
                        .background(.orange, in: Capsule())
                        .foregroundStyle(.white)
                        .padding(8)
                        .allowsHitTesting(false)
                }
            }
            .border(model.screen.takeOver ? Color.orange : Color.clear, width: 2)
        }
    }

    private var working: Bool {
        model.operation.map { !$0.finished } ?? false
    }

    // message names what the box is doing while the last frame is dimmed.
    private var message: String {
        if !model.boxRunning, let operation = model.operation, !operation.finished {
            return "\(operation.title)…"
        }
        switch model.screen.status {
        case .idle: return model.boxRunning ? "Waiting for the screen…" : "The screen appears when the box runs."
        case .connecting: return "Connecting to the box screen…"
        case .live: return ""
        case .failed(let reason): return "Screen disconnected: \(reason). Retrying…"
        }
    }
}

struct ScreenStatusBar: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: model.wording.symbol)
            Text("Agent Computer").font(.callout.bold())
            if model.screen.framebufferSize.width > 0 {
                Text("\(Int(model.screen.framebufferSize.width))×\(Int(model.screen.framebufferSize.height))")
                    .font(.caption.monospaced())
                    .foregroundStyle(.secondary)
            }
            Spacer()
            if model.screen.takeOver {
                PasswordSignInButton().controlSize(.small)
            }
            Text(model.screen.takeOver ? "take-over" : "view only")
                .font(.caption)
                .foregroundStyle(model.screen.takeOver ? .orange : .secondary)
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 6)
        .background(.bar)
    }
}

struct ScreenWindowView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        Group {
            if model.screen.poppedOut {
                LiveScreenPane()
            } else {
                ContentUnavailableView("Screen is docked", systemImage: "rectangle.split.2x1", description: Text("Use Pop out in the main window."))
            }
        }
        .frame(minWidth: 640, minHeight: 420)
        .onDisappear { model.screen.poppedOut = false }
    }
}
