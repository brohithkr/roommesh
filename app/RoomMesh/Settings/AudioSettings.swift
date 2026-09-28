import SwiftUI

struct AudioSettingsView: View {
    @Environment(AppModel.self) private var model
    @State private var devices: [FfiAudioDevice] = []
    var body: some View {
        @Bindable var settings = model.settings
        Form {
            Section("Devices") {
                Picker("Microphone", selection: $settings.inputDevice) {
                    Text("System Default").tag(String?.none)
                    ForEach(devices.filter(\.isInput), id: \.name) { Text($0.name).tag(Optional($0.name)) }
                }
                Picker("Speaker (when this Mac is the room speaker)", selection: $settings.outputDevice) {
                    Text("System Default").tag(String?.none)
                    ForEach(devices.filter(\.isOutput), id: \.name) { Text($0.name).tag(Optional($0.name)) }
                }
            }
            Section("Processing") {
                Toggle("Echo cancellation", isOn: $settings.echoCancellation)
                Toggle("Noise suppression", isOn: $settings.noiseSuppression)
                Toggle("Allow simultaneous talkers (mix two microphones)", isOn: $settings.allowSimultaneousTalkers)
                Toggle("If the room speaker disconnects, use the coordinator Mac", isOn: $settings.fallbackSpeakerToCoordinator)
            }
            Section("RoomMesh audio driver") {
                LabeledContent("Status", value: VirtualDeviceStatus.installed
                               ? "Installed (\(VirtualDeviceStatus.installedVersion ?? "unknown version"))" : "Not installed")
                LabeledContent("Meeting app setup", value: "Microphone: RoomMesh Microphone · Speaker: RoomMesh Speaker")
                HStack {
                    Button(VirtualDeviceStatus.installed ? "Reinstall…" : "Install…") {
                        do { try DriverInstaller.install() } catch { model.lastError = describe(error) }
                    }
                    if VirtualDeviceStatus.installed {
                        Button("Uninstall…") { do { try DriverInstaller.uninstall() } catch { model.lastError = describe(error) } }
                    }
                }
            }
        }
        .formStyle(.grouped)
        .onAppear { devices = listAudioDevices() }
    }
}
