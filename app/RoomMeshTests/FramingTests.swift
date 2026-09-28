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
        XCTAssertTrue(out.allSatisfy { $0.startIndex == 0 }, "decoded frames must be zero-based")
    }
    func testRejectsOversizedFrames() {
        var decoder = FrameDecoder()
        XCTAssertThrowsError(try decoder.feed(Data([0xFF, 0xFF, 0xFF, 0xFF])))
    }
    func testCustomMaxFrame() throws {
        var ok = FrameDecoder(maxFrame: 64)
        XCTAssertEqual(try ok.feed(Framing.encode(Data(repeating: 1, count: 64))), [Data(repeating: 1, count: 64)])
        var tooBig = FrameDecoder(maxFrame: 64)
        XCTAssertThrowsError(try tooBig.feed(Framing.encode(Data(repeating: 1, count: 65)))) { error in
            XCTAssertEqual(error as? FramingError, .frameTooLarge(65))
        }
    }
    /// Pre-preamble limit is small; once the preamble is parsed the limit is raised and any bytes
    /// already buffered (the start of the next, larger frame) must survive.
    func testRaisingLimitPreservesBufferedBytes() throws {
        let preamble = Preamble.make(peerId: "00abcdef01234567")
        let big = Data((0..<200).map { UInt8($0) })
        var wire = Framing.encode(preamble)
        wire.append(Framing.encode(big))
        let split = wire.startIndex + 4 + preamble.count + 4 + 50
        var decoder = FrameDecoder(maxFrame: Preamble.maxFrame)
        decoder.append(wire[..<split])
        XCTAssertEqual(try decoder.next(), preamble)
        decoder.maxFrame = Framing.maxFrame
        XCTAssertNil(try decoder.next())
        decoder.append(wire[split...])
        XCTAssertEqual(try decoder.next(), big)
        XCTAssertNil(try decoder.next())
    }
    func testSmallLimitRejectsLargeFrameAfterPreamble() throws {
        var wire = Framing.encode(Preamble.make(peerId: "00abcdef01234567"))
        wire.append(Framing.encode(Data(repeating: 0, count: 200)))
        var decoder = FrameDecoder(maxFrame: Preamble.maxFrame)
        decoder.append(wire)
        XCTAssertNotNil(try decoder.next())
        XCTAssertThrowsError(try decoder.next())
    }
    /// 1 MiB of zero-length frames in one chunk must decode in linear time.
    func testManySmallFramesInOneChunkIsLinear() throws {
        let count = (1 << 20) / 4
        let wire = Data(count: count * 4)
        var decoder = FrameDecoder()
        let start = DispatchTime.now().uptimeNanoseconds
        let out = try decoder.feed(wire)
        let elapsedMs = Double(DispatchTime.now().uptimeNanoseconds - start) / 1e6
        XCTAssertEqual(out.count, count)
        XCTAssertTrue(out.allSatisfy(\.isEmpty))
        XCTAssertLessThan(elapsedMs, 200, "decoding took \(elapsedMs) ms")
        XCTAssertEqual(try decoder.feed(Framing.encode(Data([9]))), [Data([9])], "decoder state is clean afterwards")
    }
    func testPreamble() {
        let p = Preamble.make(peerId: "00abcdef01234567")
        XCTAssertEqual(Preamble.parse(p), "00abcdef01234567")
        XCTAssertNil(Preamble.parse(Data("hello".utf8)))
        XCTAssertNil(Preamble.parse(Data("RMHELLO1:short".utf8)))
        XCTAssertLessThanOrEqual(p.count, Preamble.maxFrame)
    }
    func testPreambleRejectsUppercaseAndNonASCII() {
        XCTAssertNil(Preamble.parse(Preamble.make(peerId: "00ABCDEF01234567")))
        XCTAssertNil(Preamble.parse(Preamble.make(peerId: "０１２３４５６７８９ａｂｃｄｅｆ")), "full-width hex digits")
        XCTAssertNil(Preamble.parse(Preamble.make(peerId: "00abcdef0123456g")))
        XCTAssertNil(Preamble.parse(Preamble.make(peerId: "00abcdef0123456\u{0301}7")), "combining mark")
    }
}
