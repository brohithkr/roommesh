import SwiftUI

struct StatusBanner: View {
    @Environment(AppModel.self) private var model
    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            if model.micPermission == .denied {
                banner("Microphone access is off for RoomMesh.", action: "Open Settings") { Permissions.openPrivacySettings("Privacy_Microphone") }
            }
            if model.showsNotificationsOffBanner {
                banner("Notifications are off for RoomMesh — you'll still see invites here", action: "Open Settings") { Notifications.openSettings() }
            }
            if model.localNetworkDenied {
                banner("Local Network access is off, so nearby Macs can't be found.", action: "Open Settings") { Permissions.openPrivacySettings("Privacy_LocalNetwork") }
            }
            if let op = model.driverOperation {
                banner(op == .install ? "Installing the RoomMesh audio driver…" : "Removing the RoomMesh audio driver…", action: nil) {}
            } else if model.needsDriverInstall {
                banner(model.driverInstalled ? "An updated RoomMesh audio driver is available." : "Install the RoomMesh audio driver to use this Mac as coordinator.",
                       action: "Install…") { model.installDriver() }
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
