import SwiftUI

struct GeneralSettings: View {
    @Environment(AppModel.self) private var model
    @State private var launchAtLogin = false
    @State private var loginItemMessage: String?
    var body: some View {
        @Bindable var settings = model.settings
        Form {
            LabeledContent("This Mac", value: Host.current().localizedName ?? "Mac")
            VStack(alignment: .leading, spacing: 4) {
                Toggle("Launch RoomMesh at login", isOn: $launchAtLogin)
                    .onChange(of: launchAtLogin) { _, on in
                        guard LaunchAtLogin.action(toggleOn: on, status: LaunchAtLogin.status) != .none else { return }
                        let result = LaunchAtLogin.apply(on)
                        loginItemMessage = result.message
                        launchAtLogin = result.isOn // re-enters onChange, which is then a no-op
                    }
                if let loginItemMessage {
                    Text(loginItemMessage).font(.caption).foregroundStyle(.secondary)
                }
            }
            Toggle("Show the RoomMesh window when the app opens", isOn: $settings.showWindowAtLaunch)
            Toggle("Choose a new coordinator automatically if it disconnects", isOn: $settings.autoElectCoordinator)
            LabeledContent("Notifications") {
                HStack {
                    Text(model.notificationStatus ?? "Checking…").foregroundStyle(.secondary)
                    if model.notificationsAllowed != true {
                        Button("Open Settings") { Notifications.openSettings() }.controlSize(.small)
                    }
                }
            }
            if model.showsNotificationsOffBanner {
                Text("Invites still appear: RoomMesh plays a sound, brings its window to the front and marks the menu-bar icon.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            Section("Updates") {
                LabeledContent("Version", value: "\(model.updates.currentVersion) (core \(coreVersion()))")
                LabeledContent {
                    HStack {
                        if model.updates.isBusy { ProgressView().controlSize(.small) }
                        Button("Check for Updates") { Task { await model.updates.checkForUpdates(userInitiated: true) } }
                            .disabled(model.updates.isBusy)
                    }
                } label: {
                    VStack(alignment: .leading, spacing: 2) {
                        Text(Self.updateStatus(model.updates))
                        Text(model.updates.lastChecked.map { "Last checked \($0.formatted(date: .abbreviated, time: .shortened))" } ?? "Never checked")
                            .font(.caption).foregroundStyle(.secondary)
                    }
                }
                Toggle("Check for updates automatically", isOn: $settings.checkForUpdatesAutomatically)
                Toggle("Include pre-releases", isOn: $settings.includePrereleases)
            }
        }
        .formStyle(.grouped)
        .onAppear {
            Notifications.shared.refreshStatus()
            // The state can change outside the app (System Settings › Login Items).
            let status = LaunchAtLogin.status
            launchAtLogin = LaunchAtLogin.isOn(status)
            loginItemMessage = status == .requiresApproval ? LaunchAtLogin.approvalMessage : nil
        }
    }

    static func updateStatus(_ u: UpdateChecker) -> String {
        switch u.state {
        case .idle: "RoomMesh checks GitHub for new releases."
        case .checking: "Checking…"
        case .upToDate: "RoomMesh is up to date."
        case .available(let update): "RoomMesh \(update.version) is available."
        case .downloading: "Downloading the update…"
        case .readyToInstall: "The update is ready to install."
        case .failed(let message): message
        }
    }
}
