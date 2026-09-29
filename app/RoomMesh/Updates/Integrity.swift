import CryptoKit
import Foundation

enum UpdateError: Error, Equatable, LocalizedError {
    case http(Int)
    case rateLimited
    case noPackage
    case noChecksums
    case missingChecksum(String)
    case checksumMismatch(String)

    var errorDescription: String? {
        switch self {
        case .http(let code): "GitHub returned an error (HTTP \(code)). Try again later."
        case .rateLimited: "GitHub is limiting requests from this network right now. Try again in a while."
        case .noPackage: "This release has no installer package. Download it from GitHub instead."
        case .noChecksums: "This release has no SHA256SUMS.txt, so the download can't be verified. Download it from GitHub instead."
        case .missingChecksum(let name): "SHA256SUMS.txt has no checksum for \(name), so it can't be verified. Download it from GitHub instead."
        case .checksumMismatch(let name): "The downloaded \(name) doesn't match its published SHA-256 checksum, so it was deleted. Try again."
        }
    }
}

enum SHA256Sums {
    /// Parses `sha256sum`/`shasum -a 256` output (`<hex>  <name>`, `*` marking binary mode) into
    /// file name → lower-case hex. Lines that aren't checksums are skipped.
    static func parse(_ text: String) -> [String: String] {
        var sums: [String: String] = [:]
        for raw in text.split(whereSeparator: \.isNewline) {
            let line = raw.trimmingCharacters(in: .whitespaces)
            guard !line.hasPrefix("#"), let space = line.firstIndex(where: \.isWhitespace) else { continue }
            let hash = line[..<space].lowercased()
            guard hash.count == 64, hash.allSatisfy(\.isHexDigit) else { continue }
            var name = line[space...].trimmingCharacters(in: .whitespaces)
            if name.hasPrefix("*") { name.removeFirst() }
            if name.hasPrefix("./") { name.removeFirst(2) }
            guard !name.isEmpty else { continue }
            sums[name] = hash
        }
        return sums
    }

    /// Lower-case hex SHA-256 of a file, read in chunks.
    static func hash(of file: URL) throws -> String {
        let handle = try FileHandle(forReadingFrom: file)
        defer { try? handle.close() }
        var hasher = SHA256()
        while let chunk = try handle.read(upToCount: 1 << 20), !chunk.isEmpty { hasher.update(data: chunk) }
        return hasher.finalize().map { String(format: "%02x", $0) }.joined()
    }

    /// Throws unless `sums` lists `name` with the file's SHA-256.
    static func verify(file: URL, name: String, sums: [String: String]) throws {
        guard let expected = sums[name] else { throw UpdateError.missingChecksum(name) }
        guard try hash(of: file) == expected else { throw UpdateError.checksumMismatch(name) }
    }
}

enum PackageSignature {
    /// The `Developer ID Installer: …` identity `pkgutil --check-signature` reports, or `nil` for an
    /// unsigned (or otherwise signed) package.
    static func developerID(fromPkgutilOutput output: String) -> String? {
        guard !output.contains("Status: no signature") else { return nil }
        for line in output.split(whereSeparator: \.isNewline) {
            if let r = line.range(of: "Developer ID Installer:") {
                return String(line[r.lowerBound...]).trimmingCharacters(in: .whitespaces)
            }
        }
        return nil
    }

    /// Runs `pkgutil --check-signature` (blocking; call off the main thread).
    static func developerID(of pkg: URL) -> String? {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: "/usr/sbin/pkgutil")
        p.arguments = ["--check-signature", pkg.path]
        let out = Pipe()
        p.standardOutput = out
        p.standardError = FileHandle.nullDevice
        do { try p.run() } catch { return nil }
        let data = out.fileHandleForReading.readDataToEndOfFile()
        p.waitUntilExit()
        return developerID(fromPkgutilOutput: String(decoding: data, as: UTF8.self))
    }
}
