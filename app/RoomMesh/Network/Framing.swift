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

struct FrameDecoder {
    private var buffer = Data()
    mutating func feed(_ chunk: Data) throws -> [Data] {
        buffer.append(chunk)
        var frames: [Data] = []
        while buffer.count >= 4 {
            let len = Int(buffer.prefix(4).reduce(UInt32(0)) { ($0 << 8) | UInt32($1) })
            guard len <= Framing.maxFrame else { throw FramingError.frameTooLarge(len) }
            guard buffer.count >= 4 + len else { break }
            frames.append(Data(buffer.dropFirst(4).prefix(len)))
            buffer = Data(buffer.dropFirst(4 + len))
        }
        return frames
    }
}

/// First frame on every dialed control connection: identifies the dialer to the acceptor.
enum Preamble {
    static let prefix = "RMHELLO1:"
    static func make(peerId: String) -> Data { Data((prefix + peerId).utf8) }
    static func parse(_ frame: Data) -> String? {
        guard let s = String(data: frame, encoding: .utf8), s.hasPrefix(prefix) else { return nil }
        let id = String(s.dropFirst(prefix.count))
        return id.count == 16 && id.allSatisfy(\.isHexDigit) ? id : nil
    }
}
