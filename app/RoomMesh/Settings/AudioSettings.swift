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
                    ForEach(DeviceRow.rows(devices.filter(\.isInput))) { Text($0.name).tag(Optional($0.name)) }
                }
                Picker("Speaker (when this Mac is the room speaker)", selection: $settings.outputDevice) {
                    Text("System Default").tag(String?.none)
                    ForEach(DeviceRow.rows(devices.filter(\.isOutput))) { Text($0.name).tag(Optional($0.name)) }
                }
            }
            Section("Processing") {
                Toggle("Echo cancellation", isOn: $settings.echoCancellation)
                Toggle("Noise suppression", isOn: $settings.noiseSuppression)
                Toggle("Allow simultaneous talkers (mix two microphones)", isOn: $settings.allowSimultaneousTalkers)
                Toggle("If the room speaker disconnects, use the coordinator Mac", isOn: $settings.fallbackSpeakerToCoordinator)
            }
            Section("RoomMesh audio driver") {
                LabeledContent("Status", value: model.driverOperation?.progressLabel
                               ?? (model.driverInstalled ? "Installed (\(model.installedDriverVersion ?? "unknown version"))" : "Not installed"))
                LabeledContent("Meeting app setup", value: "Microphone: RoomMesh Microphone · Speaker: RoomMesh Speaker")
                HStack {
                    Button(model.driverInstalled ? "Reinstall…" : "Install…") { model.installDriver() }
                    if model.driverInstalled {
                        Button("Uninstall…") { model.uninstallDriver() }
                    }
                    if model.driverOperation != nil { ProgressView().controlSize(.small) }
                }
                .disabled(model.driverOperation != nil)
                if let err = model.lastError {
                    Text(err).font(.caption).foregroundStyle(.red)
                }
            }
        }
        .formStyle(.grouped)
        .onAppear { devices = listAudioDevices(); model.refreshStatus() }
    }
}

/// Device names aren't unique (two identical USB mics), so rows are keyed by index + name.
struct DeviceRow: Identifiable {
    let id: String
    let name: String
    static func rows(_ devices: [FfiAudioDevice]) -> [DeviceRow] {
        devices.enumerated().map { DeviceRow(id: "\($0.offset):\($0.element.name)", name: $0.element.name) }
    }
}
