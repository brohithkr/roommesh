import SwiftUI

struct SettingsView: View {
    @Environment(AppModel.self) private var model
    var body: some View {
        @Bindable var model = model
        TabView(selection: $model.settingsTab) {
            GeneralSettings().tabItem { Label("General", systemImage: "gearshape") }.tag(SettingsTab.general)
            AudioSettingsView().tabItem { Label("Audio", systemImage: "waveform") }.tag(SettingsTab.audio)
            NetworkSettings().tabItem { Label("Network", systemImage: "network") }.tag(SettingsTab.network)
            AdvancedSettings().tabItem { Label("Advanced", systemImage: "slider.horizontal.3") }.tag(SettingsTab.advanced)
        }
        .frame(width: 560, height: 420)
    }
}
