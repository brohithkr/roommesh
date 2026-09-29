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
            LabeledContent("Version", value: "\(Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? "?") (core \(coreVersion()))")
        }
        .formStyle(.grouped)
        .onAppear {
            // The state can change outside the app (System Settings › Login Items).
            let status = LaunchAtLogin.status
            launchAtLogin = LaunchAtLogin.isOn(status)
            loginItemMessage = status == .requiresApproval ? LaunchAtLogin.approvalMessage : nil
        }
    }
}
