import Foundation

/// Length-prefixed frames on the TCP control channel: 4-byte big-endian length + payload.
enum Framing {
    static let maxFrame = 1 << 20
    static func encode(_ payload: Data) -> Data {
        var len = UInt32(payload.count).bigEndian
        var d = Data(bytes: &len, count: 4)
        d.append(payload)
        return d
    }
}

enum FramingError: Error, Equatable { case frameTooLarge(Int) }

/// Incremental decoder. Parsing walks an offset over the buffer; consumed bytes are compacted with
/// a single `removeSubrange` per `feed` (or on the next `append`), so decoding is linear in input size.
/// `maxFrame` may be raised between frames without losing buffered bytes.
struct FrameDecoder {
    var maxFrame: Int
    private var buffer = Data()
    /// Bytes at the front of `buffer` (relative to `buffer.startIndex`) that have been parsed already.
    private var consumed = 0

    init(maxFrame: Int = Framing.maxFrame) { self.maxFrame = maxFrame }

    mutating func append(_ chunk: Data) {
        compact()
        buffer.append(chunk)
    }

    /// Next complete frame, or nil if more bytes are needed. Throws on a header above `maxFrame`.
    mutating func next() throws -> Data? {
        let start = buffer.startIndex + consumed
        guard buffer.endIndex - start >= 4 else { return nil }
        let len = Int(buffer[start]) << 24 | Int(buffer[start + 1]) << 16 | Int(buffer[start + 2]) << 8 | Int(buffer[start + 3])
        guard len <= maxFrame else { throw FramingError.frameTooLarge(len) }
        let body = start + 4
        guard buffer.endIndex - body >= len else { return nil }
        consumed += 4 + len
        return len == 0 ? Data() : Data(buffer[body ..< body + len])
    }

    mutating func feed(_ chunk: Data) throws -> [Data] {
        append(chunk)
        var frames: [Data] = []
        while let f = try next() { frames.append(f) }
        compact()
        return frames
    }

    private mutating func compact() {
        guard consumed > 0 else { return }
        buffer.removeSubrange(buffer.startIndex ..< buffer.startIndex + consumed)
        consumed = 0
    }
}

/// First frame on every dialed control connection: identifies the dialer to the acceptor.
enum Preamble {
    static let prefix = "RMHELLO1:"
    /// Frame limit on an inbound connection until its preamble has been parsed.
    static let maxFrame = 64
    static func make(peerId: String) -> Data { Data((prefix + peerId).utf8) }
    /// Accepts exactly `prefix` followed by 16 lowercase ASCII hex digits (`0-9a-f`).
    static func parse(_ frame: Data) -> String? {
        let head = Data(prefix.utf8)
        guard frame.count == head.count + 16, frame.starts(with: head) else { return nil }
        let id = frame.dropFirst(head.count)
        guard id.allSatisfy({ (0x30...0x39).contains($0) || (0x61...0x66).contains($0) }) else { return nil }
        return String(decoding: id, as: UTF8.self)
    }
}
