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
}
