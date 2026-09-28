import AppKit
import AVFoundation

enum MicPermission { case granted, denied, undetermined }

@MainActor
enum Permissions {
    static var microphone: MicPermission {
        switch AVCaptureDevice.authorizationStatus(for: .audio) {
        case .authorized: .granted
        case .notDetermined: .undetermined
        default: .denied
        }
    }
    static func requestMicrophone() async -> Bool { await AVCaptureDevice.requestAccess(for: .audio) }
    /// anchor: "Privacy_Microphone" or "Privacy_LocalNetwork"
    static func openPrivacySettings(_ anchor: String) {
        if let url = URL(string: "x-apple.systempreferences:com.apple.preference.security?\(anchor)") { NSWorkspace.shared.open(url) }
    }
}
