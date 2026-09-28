import XCTest
@testable import RoomMesh

final class IntegrationHelpersTests: XCTestCase {
    func testShellQuoting() {
        XCTAssertEqual(DriverInstaller.shellQuote("/a b/it's"), "'/a b/it'\\''s'")
    }
    func testAppleScriptEscaping() {
        XCTAssertEqual(DriverInstaller.appleScript(for: "echo \"hi\" \\"), "do shell script \"echo \\\"hi\\\" \\\\\" with administrator privileges")
    }
    func testUnknownDeviceUIDIsAbsent() {
        XCTAssertFalse(VirtualDeviceStatus.deviceExists(uid: "definitely-not-a-device-\(UUID())"))
    }
    // MARK: driver install status

    /// Builds a fake `RoomMesh.driver` bundle with the given executable bytes and version.
    private func makeDriver(_ bytes: [UInt8]?, version: String) throws -> URL {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("rm-driver-\(UUID())/RoomMesh.driver")
        let contents = root.appendingPathComponent("Contents")
        try FileManager.default.createDirectory(at: contents.appendingPathComponent("MacOS"), withIntermediateDirectories: true)
        let plist: NSDictionary = ["CFBundleShortVersionString": version]
        XCTAssertTrue(plist.write(to: contents.appendingPathComponent("Info.plist"), atomically: true))
        if let bytes { try Data(bytes).write(to: VirtualDeviceStatus.executableURL(inDriver: root)) }
        addTeardownBlock { try? FileManager.default.removeItem(at: root.deletingLastPathComponent()) }
        return root
    }
    func testDriverComparisonUsesExecutableHash() throws {
        let bundled = try makeDriver([1, 2, 3], version: "1.0.0")
        XCTAssertFalse(DriverInstaller.driverDiffers(installed: try makeDriver([1, 2, 3], version: "1.0.0"), bundled: bundled))
        XCTAssertTrue(DriverInstaller.driverDiffers(installed: try makeDriver([9, 9], version: "1.0.0"), bundled: bundled),
                      "a rebuilt driver with the same version string is still an update")
        XCTAssertFalse(DriverInstaller.driverDiffers(installed: try makeDriver([1, 2, 3], version: "0.9"), bundled: bundled),
                       "identical executables win over a differing version string")
    }
    func testDriverComparisonFallsBackToVersionWhenUnreadable() throws {
        let bundled = try makeDriver([1, 2, 3], version: "1.0.0")
        XCTAssertFalse(DriverInstaller.driverDiffers(installed: try makeDriver(nil, version: "1.0.0"), bundled: bundled))
        XCTAssertTrue(DriverInstaller.driverDiffers(installed: try makeDriver(nil, version: "0.9"), bundled: bundled))
        let missing = FileManager.default.temporaryDirectory.appendingPathComponent("nope-\(UUID()).driver")
        XCTAssertTrue(DriverInstaller.driverDiffers(installed: missing, bundled: bundled), "not installed at all")
    }
    func testInstalledVersionIsReadFreshFromDisk() throws {
        let driver = try makeDriver([1], version: "1.0.0")
        XCTAssertEqual(VirtualDeviceStatus.version(ofDriver: driver), "1.0.0")
        let plist: NSDictionary = ["CFBundleShortVersionString": "2.0.0"]
        XCTAssertTrue(plist.write(to: driver.appendingPathComponent("Contents/Info.plist"), atomically: true))
        XCTAssertEqual(VirtualDeviceStatus.version(ofDriver: driver), "2.0.0", "no Bundle caching")
    }
    func testHashCacheNoticesRewrites() throws {
        let driver = try makeDriver([1, 2, 3], version: "1.0.0")
        let exe = VirtualDeviceStatus.executableURL(inDriver: driver)
        let first = VirtualDeviceStatus.executableHash(exe)
        XCTAssertNotNil(first)
        try Data([4, 5, 6, 7]).write(to: exe)
        XCTAssertNotEqual(VirtualDeviceStatus.executableHash(exe), first)
    }
    func testNeedsInstallRules() throws {
        let bundled = try makeDriver([1], version: "1.0.0")
        let same = try makeDriver([1], version: "1.0.0")
        XCTAssertFalse(DriverInstaller.needsInstall(bundled: nil, installed: same, devicesPresent: false), "dev build without a driver never nags")
        XCTAssertTrue(DriverInstaller.needsInstall(bundled: bundled, installed: same, devicesPresent: false), "devices missing")
        XCTAssertFalse(DriverInstaller.needsInstall(bundled: bundled, installed: same, devicesPresent: true))
    }
    func testInstallerScriptsAndCancellation() {
        XCTAssertTrue(DriverInstaller.restartCoreAudio.contains("launchctl kickstart -k system/com.apple.audio.coreaudiod"))
        XCTAssertTrue(DriverInstaller.restartCoreAudio.contains("killall -9 coreaudiod"), "fallback")
        XCTAssertTrue(DriverInstaller.isUserCancel("0:103: execution error: User canceled. (-128)\n"))
        XCTAssertFalse(DriverInstaller.isUserCancel("0:103: execution error: rm: /x: Permission denied (1)"))
        XCTAssertEqual(DriverInstaller.errorMessage(fromOsascript: "0:57: execution error: ditto: boom (1)\n"), "ditto: boom")
    }
    // MARK: launch at login
    func testLaunchAtLoginActionIsIdempotent() {
        // Re-asserting the current state (e.g. the revert after an error) must not re-register.
        XCTAssertEqual(LaunchAtLogin.action(toggleOn: true, status: .enabled), .none)
        XCTAssertEqual(LaunchAtLogin.action(toggleOn: true, status: .requiresApproval), .none)
        XCTAssertEqual(LaunchAtLogin.action(toggleOn: false, status: .notRegistered), .none)
        XCTAssertEqual(LaunchAtLogin.action(toggleOn: false, status: .notFound), .none)
        XCTAssertEqual(LaunchAtLogin.action(toggleOn: true, status: .notRegistered), .register)
        XCTAssertEqual(LaunchAtLogin.action(toggleOn: true, status: .notFound), .register)
        XCTAssertEqual(LaunchAtLogin.action(toggleOn: false, status: .enabled), .unregister)
        XCTAssertEqual(LaunchAtLogin.action(toggleOn: false, status: .requiresApproval), .unregister)
        XCTAssertTrue(LaunchAtLogin.isOn(.requiresApproval), "registered, pending approval")
        XCTAssertFalse(LaunchAtLogin.isOn(.notFound))
    }
}
