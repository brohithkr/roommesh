import XCTest
@testable import RoomMesh

final class FramingTests: XCTestCase {
    func testRoundTripAcrossArbitraryChunking() throws {
        let frames = [Data([1, 2, 3]), Data(), Data(repeating: 7, count: 70_000)]
        var wire = Data()
        frames.forEach { wire.append(Framing.encode($0)) }
        var decoder = FrameDecoder()
        var out: [Data] = []
        var i = wire.startIndex
        while i < wire.endIndex {
            let j = min(i + 997, wire.endIndex)
            out += try decoder.feed(wire[i..<j])
            i = j
        }
        XCTAssertEqual(out, frames)
    }
    func testRejectsOversizedFrames() {
        var decoder = FrameDecoder()
        XCTAssertThrowsError(try decoder.feed(Data([0xFF, 0xFF, 0xFF, 0xFF])))
    }
    func testPreamble() {
        let p = Preamble.make(peerId: "00abcdef01234567")
        XCTAssertEqual(Preamble.parse(p), "00abcdef01234567")
        XCTAssertNil(Preamble.parse(Data("hello".utf8)))
        XCTAssertNil(Preamble.parse(Data("RMHELLO1:short".utf8)))
    }
}
