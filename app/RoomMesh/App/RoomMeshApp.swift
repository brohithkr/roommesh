import SwiftUI

@main
struct RoomMeshApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    var body: some Scene {
        MenuBarExtra {
            MenuBarContent().environment(AppModel.shared)
        } label: {
            MenuBarIcon().environment(AppModel.shared)
        }
        .menuBarExtraStyle(.menu)
        Settings {
            SettingsView().environment(AppModel.shared)
        }
    }
}
