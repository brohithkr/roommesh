import AppKit
import SwiftUI

@MainActor
final class UpdateWindowController: NSObject, NSWindowDelegate {
    static let shared = UpdateWindowController()
    private var window: NSWindow?

    func show() {
        if window == nil {
            let host = NSHostingController(rootView: UpdateView().environment(AppModel.shared))
            let w = NSWindow(contentViewController: host)
            w.title = "Software Update"
            w.styleMask = [.titled, .closable]
            w.isReleasedWhenClosed = false
            w.center()
            w.delegate = self
            window = w
        }
        NSApp.activate()
        window?.makeKeyAndOrderFront(nil)
        window?.orderFrontRegardless()
    }
    func close() { window?.close() }
}

/// Current vs new version, the release notes and Download & Install / Later / View on GitHub.
struct UpdateView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        let updates = model.updates
        VStack(alignment: .leading, spacing: 14) {
            HStack(alignment: .top, spacing: 14) {
                Image(nsImage: NSApp.applicationIconImage).resizable().frame(width: 56, height: 56)
                VStack(alignment: .leading, spacing: 4) {
                    Text(headline(updates)).font(.headline)
                    Text(subheadline(updates)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                }
            }
            if let update = updates.update {
                releaseNotes(update)
                status(updates)
                HStack {
                    Button("View on GitHub") { NSWorkspace.shared.open(update.pageURL) }
                    Spacer()
                    Button("Later") { updates.later(); UpdateWindowController.shared.close() }
                        .keyboardShortcut(.cancelAction)
                    primaryButton(updates)
                }
            } else {
                noUpdateContent(updates)
            }
        }
        .padding(20)
        .frame(width: 480)
    }

    private func headline(_ u: UpdateChecker) -> String {
        if let update = u.update { return "RoomMesh \(update.version) is available" }
        switch u.state {
        case .checking, .idle: return "Checking for updates…"
        case .upToDate: return "RoomMesh is up to date"
        case .failed: return "Couldn't check for updates"
        default: return "Software Update"
        }
    }
    private func subheadline(_ u: UpdateChecker) -> String {
        if let update = u.update {
            return "You have \(u.currentVersion)." + (update.isPrerelease ? " This is a pre-release." : "")
        }
        switch u.state {
        case .upToDate: return "Version \(u.currentVersion) is the newest version available."
        case .failed(let message): return message
        default: return "You have \(u.currentVersion)."
        }
    }

    private func releaseNotes(_ update: AvailableUpdate) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(update.title).font(.subheadline.bold())
            ScrollView {
                Text(verbatim: update.notes.isEmpty ? "No release notes." : update.notes)
                    .font(.callout)
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(10)
            }
            .frame(height: 200)
            .background(Color(nsColor: .textBackgroundColor), in: RoundedRectangle(cornerRadius: 6))
            .overlay(RoundedRectangle(cornerRadius: 6).strokeBorder(.separator))
        }
    }

    @ViewBuilder private func status(_ u: UpdateChecker) -> some View {
        switch u.state {
        case .downloading(let fraction):
            VStack(alignment: .leading, spacing: 4) {
                ProgressView(value: fraction)
                Text("Downloading… \(Int((fraction * 100).rounded()))%").font(.caption).foregroundStyle(.secondary)
            }
        case .readyToInstall:
            VStack(alignment: .leading, spacing: 2) {
                Label("Downloaded and verified (SHA-256).", systemImage: "checkmark.seal").font(.callout)
                Text(u.packageSignature.map { "Signed: \($0)." }
                     ?? "This build isn't signed with a Developer ID, so macOS may ask you to confirm before it opens.")
                    .font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                Text("The installer quits RoomMesh and reopens it when it's done.")
                    .font(.caption).foregroundStyle(.secondary)
            }
        case .failed(let message):
            Label(message, systemImage: "exclamationmark.triangle.fill")
                .font(.callout).foregroundStyle(.orange).fixedSize(horizontal: false, vertical: true)
        default:
            EmptyView()
        }
    }

    @ViewBuilder private func primaryButton(_ u: UpdateChecker) -> some View {
        switch u.state {
        case .readyToInstall:
            Button("Install…") { u.install() }.keyboardShortcut(.defaultAction).buttonStyle(.borderedProminent)
        case .failed:
            Button("Try Again") { Task { await u.downloadAndInstall() } }.keyboardShortcut(.defaultAction)
        default:
            Button("Download & Install") { Task { await u.downloadAndInstall() } }
                .keyboardShortcut(.defaultAction).buttonStyle(.borderedProminent)
                .disabled(u.isBusy)
        }
    }

    @ViewBuilder private func noUpdateContent(_ u: UpdateChecker) -> some View {
        HStack {
            if case .checking = u.state { ProgressView().controlSize(.small) }
            Spacer()
            if case .failed = u.state {
                Button("Try Again") { Task { await u.checkForUpdates(userInitiated: true) } }
            }
            Button("OK") { UpdateWindowController.shared.close() }.keyboardShortcut(.defaultAction)
        }
    }
}
