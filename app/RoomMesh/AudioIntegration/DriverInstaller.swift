import Foundation

enum DriverOperation: Equatable, Sendable {
    case install, uninstall
    var progressLabel: String { self == .install ? "Installing…" : "Uninstalling…" }
}

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
    static var bundledVersion: String? { bundledDriverURL.flatMap(VirtualDeviceStatus.version(ofDriver:)) }

    /// Only when this build carries a driver (dev builds without `driver/build` never nag).
    static var needsInstall: Bool {
        needsInstall(bundled: bundledDriverURL, installed: VirtualDeviceStatus.installedDriverURL,
                     devicesPresent: VirtualDeviceStatus.installed)
    }
    static func needsInstall(bundled: URL?, installed: URL, devicesPresent: Bool) -> Bool {
        guard let bundled else { return false }
        return !devicesPresent || driverDiffers(installed: installed, bundled: bundled)
    }
    /// Drivers are rebuilt without bumping the version, so compare the executables' SHA-256;
    /// fall back to the version string when either executable can't be read.
    static func driverDiffers(installed: URL, bundled: URL) -> Bool {
        if let a = VirtualDeviceStatus.executableHash(VirtualDeviceStatus.executableURL(inDriver: installed)),
           let b = VirtualDeviceStatus.executableHash(VirtualDeviceStatus.executableURL(inDriver: bundled)) {
            return a != b
        }
        return VirtualDeviceStatus.version(ofDriver: installed) != VirtualDeviceStatus.version(ofDriver: bundled)
    }

    static func shellQuote(_ s: String) -> String { "'" + s.replacingOccurrences(of: "'", with: "'\\''") + "'" }
    static func appleScript(for shell: String) -> String {
        let escaped = shell.replacingOccurrences(of: "\\", with: "\\\\").replacingOccurrences(of: "\"", with: "\\\"")
        return "do shell script \"\(escaped)\" with administrator privileges"
    }
    /// Restarts coreaudiod so it loads (or drops) the plug-in.
    static let restartCoreAudio =
        "(/bin/launchctl kickstart -k system/com.apple.audio.coreaudiod || /usr/bin/killall -9 coreaudiod || true)"

    /// Runs `op` behind the admin prompt. Blocking: call off the main thread.
    /// Returns false if the user cancelled the prompt.
    static func perform(_ op: DriverOperation) throws -> Bool {
        let dst = shellQuote(VirtualDeviceStatus.installedDriverURL.path)
        switch op {
        case .install:
            guard let src = bundledDriverURL else { throw InstallError.missingBundle }
            return try runAdmin("rm -rf \(dst) && /usr/bin/ditto \(shellQuote(src.path)) \(dst) && /usr/sbin/chown -R root:wheel \(dst) && \(restartCoreAudio)")
        case .uninstall:
            return try runAdmin("rm -rf \(dst) && \(restartCoreAudio)")
        }
    }

    private static func runAdmin(_ shell: String) throws -> Bool {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: "/usr/bin/osascript")
        p.arguments = ["-e", appleScript(for: shell)]
        let stderr = Pipe()
        p.standardError = stderr
        p.standardOutput = FileHandle.nullDevice
        do { try p.run() } catch { throw InstallError.failed("Could not start the installer: \(error.localizedDescription)") }
        let output = String(data: stderr.fileHandleForReading.readDataToEndOfFile(), encoding: .utf8) ?? ""
        p.waitUntilExit()
        if p.terminationStatus == 0 { return true }
        if isUserCancel(output) { return false }
        throw InstallError.failed(errorMessage(fromOsascript: output))
    }

    /// osascript reports a dismissed admin prompt as error -128.
    static func isUserCancel(_ stderr: String) -> Bool { stderr.contains("(-128)") }
    /// "0:57: execution error: ditto: boom (1)" → "ditto: boom"
    static func errorMessage(fromOsascript stderr: String) -> String {
        var s = stderr.trimmingCharacters(in: .whitespacesAndNewlines)
        if let r = s.range(of: "execution error: ") { s = String(s[r.upperBound...]) }
        if let r = s.range(of: #"\s*\(-?\d+\)$"#, options: .regularExpression) { s.removeSubrange(r) }
        return s.isEmpty ? "Installation failed." : s
    }
}
