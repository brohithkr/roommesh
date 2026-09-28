import XCTest
@testable import RoomMesh

final class SmokeTests: XCTestCase {
    func testCoreLinksAndGeneratesPeerIds() {
        XCTAssertFalse(coreVersion().isEmpty)
        let a = generatePeerId(), b = generatePeerId()
        XCTAssertEqual(a.count, 16)
        XCTAssertNotEqual(a, b)
    }
}
