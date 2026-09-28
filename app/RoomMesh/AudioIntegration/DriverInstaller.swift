import AppKit

enum DriverInstaller {
    enum InstallError: LocalizedError {
        case missingBundle, failed(String)
        var errorDescription: String? {
            switch self {
            case .missingBundle: "This copy of RoomMesh does not contain the audio driver."
            case .failed(let m): m
            }
        }
    }
    static var bundledDriverURL: URL? { Bundle.main.url(forResource: "RoomMesh", withExtension: "driver") }
    static var bundledVersion: String? { bundledDriverURL.flatMap { Bundle(url: $0)?.infoDictionary?["CFBundleShortVersionString"] as? String } }
    /// Only when this build carries a driver (dev builds without `driver/build` never nag).
    static var needsInstall: Bool {
        guard bundledDriverURL != nil else { return false }
        return !VirtualDeviceStatus.installed || VirtualDeviceStatus.installedVersion != bundledVersion
    }

    static func shellQuote(_ s: String) -> String { "'" + s.replacingOccurrences(of: "'", with: "'\\''") + "'" }
    static func appleScript(for shell: String) -> String {
        let escaped = shell.replacingOccurrences(of: "\\", with: "\\\\").replacingOccurrences(of: "\"", with: "\\\"")
        return "do shell script \"\(escaped)\" with administrator privileges"
    }

    /// Copies the bundled driver into /Library/Audio/Plug-Ins/HAL (admin prompt) and restarts coreaudiod.
    @MainActor static func install() throws {
        guard let src = bundledDriverURL else { throw InstallError.missingBundle }
        let dst = VirtualDeviceStatus.installedDriverURL.path
        try runAdmin("rm -rf \(shellQuote(dst)) && /usr/bin/ditto \(shellQuote(src.path)) \(shellQuote(dst)) && /usr/sbin/chown -R root:wheel \(shellQuote(dst)) && (/usr/bin/killall -9 coreaudiod || true)")
    }
    @MainActor static func uninstall() throws {
        try runAdmin("rm -rf \(shellQuote(VirtualDeviceStatus.installedDriverURL.path)) && (/usr/bin/killall -9 coreaudiod || true)")
    }
    @MainActor private static func runAdmin(_ shell: String) throws {
        var err: NSDictionary?
        guard let script = NSAppleScript(source: appleScript(for: shell)) else { throw InstallError.failed("Could not prepare the installer.") }
        script.executeAndReturnError(&err)
        if let err { throw InstallError.failed(err[NSAppleScript.errorMessage] as? String ?? "Installation failed.") }
    }
}
