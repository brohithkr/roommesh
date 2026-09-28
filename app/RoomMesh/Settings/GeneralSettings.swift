import SwiftUI

struct GeneralSettings: View {
    @Environment(AppModel.self) private var model
    @State private var launchAtLogin = LaunchAtLogin.isEnabled
    var body: some View {
        @Bindable var settings = model.settings
        Form {
            LabeledContent("This Mac", value: Host.current().localizedName ?? "Mac")
            Toggle("Launch RoomMesh at login", isOn: $launchAtLogin)
                .onChange(of: launchAtLogin) { _, on in
                    do { try LaunchAtLogin.set(on) } catch { model.lastError = describe(error); launchAtLogin = LaunchAtLogin.isEnabled }
                }
            Toggle("Show the RoomMesh window when the app opens", isOn: $settings.showWindowAtLaunch)
            Toggle("Choose a new coordinator automatically if it disconnects", isOn: $settings.autoElectCoordinator)
            LabeledContent("Version", value: "\(Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? "?") (core \(coreVersion()))")
        }
        .formStyle(.grouped)
    }
}
