import SwiftUI

struct StatusBanner: View {
    @Environment(AppModel.self) private var model
    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            if Permissions.microphone == .denied {
                banner("Microphone access is off for RoomMesh.", action: "Open Settings") { Permissions.openPrivacySettings("Privacy_Microphone") }
            }
            if model.localNetworkDenied {
                banner("Local Network access is off, so nearby Macs can't be found.", action: "Open Settings") { Permissions.openPrivacySettings("Privacy_LocalNetwork") }
            }
            if DriverInstaller.needsInstall {
                banner(model.driverInstalled ? "An updated RoomMesh audio driver is available." : "Install the RoomMesh audio driver to use this Mac as coordinator.",
                       action: "Install…") {
                    do { try DriverInstaller.install() } catch { model.lastError = describe(error) }
                }
            } else if model.isLocalCoordinator && !model.virtualDeviceAvailable {
                banner("RoomMesh Microphone is not reachable yet. If this persists, reinstall the driver.", action: nil) {}
            }
            if let err = model.lastError {
                banner(err, action: "Dismiss") { model.lastError = nil }
            }
            if let notice = model.notice {
                // Transient, informational: neutral styling, no action; AppModel clears it after a few seconds.
                HStack(alignment: .top) {
                    Image(systemName: "info.circle").foregroundStyle(.secondary)
                    Text(notice).font(.callout).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                    Spacer()
                }
                .padding(8)
                .background(Color.secondary.opacity(0.08), in: RoundedRectangle(cornerRadius: 8))
            }
        }
    }
    private func banner(_ text: String, action: String?, _ perform: @escaping () -> Void) -> some View {
        HStack(alignment: .top) {
            Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.orange)
            Text(text).font(.callout).fixedSize(horizontal: false, vertical: true)
            Spacer()
            if let action { Button(action, action: perform).controlSize(.small) }
        }
        .padding(8)
        .background(Color.orange.opacity(0.08), in: RoundedRectangle(cornerRadius: 8))
    }
}
