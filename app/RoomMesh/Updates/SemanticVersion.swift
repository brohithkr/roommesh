import Foundation

/// A semantic version (`MAJOR.MINOR.PATCH[-PRERELEASE][+BUILD]`), compared by semver precedence:
/// numerically per component (so 1.10.0 > 1.9.0), a pre-release before its release, build metadata
/// ignored. A leading `v` (as in release tags) and a missing patch (`1.2`) are accepted.
struct SemanticVersion: Comparable, Hashable, CustomStringConvertible, Sendable {
    let major: Int
    let minor: Int
    let patch: Int
    let prerelease: [String]

    init(_ major: Int, _ minor: Int, _ patch: Int, prerelease: [String] = []) {
        (self.major, self.minor, self.patch, self.prerelease) = (major, minor, patch, prerelease)
    }

    init?(_ string: String) {
        var s = Substring(string.trimmingCharacters(in: .whitespacesAndNewlines))
        if s.first == "v" || s.first == "V" { s = s.dropFirst() }
        if let plus = s.firstIndex(of: "+") { s = s[..<plus] }
        var pre: [String] = []
        if let dash = s.firstIndex(of: "-") {
            pre = s[s.index(after: dash)...].split(separator: ".", omittingEmptySubsequences: false).map(String.init)
            let valid = pre.allSatisfy { !$0.isEmpty && $0.allSatisfy { $0.isASCII && ($0.isLetter || $0.isNumber || $0 == "-") } }
            guard valid else { return nil }
            s = s[..<dash]
        }
        let parts = s.split(separator: ".", omittingEmptySubsequences: false)
        guard (2...3).contains(parts.count) else { return nil }
        var numbers: [Int] = []
        for part in parts {
            guard !part.isEmpty, part.allSatisfy({ $0.isASCII && $0.isNumber }), let n = Int(part) else { return nil }
            numbers.append(n)
        }
        self.init(numbers[0], numbers[1], numbers.count > 2 ? numbers[2] : 0, prerelease: pre)
    }

    var isPrerelease: Bool { !prerelease.isEmpty }
    var description: String {
        "\(major).\(minor).\(patch)" + (prerelease.isEmpty ? "" : "-" + prerelease.joined(separator: "."))
    }

    static func < (a: SemanticVersion, b: SemanticVersion) -> Bool {
        if (a.major, a.minor, a.patch) != (b.major, b.minor, b.patch) {
            return (a.major, a.minor, a.patch) < (b.major, b.minor, b.patch)
        }
        // A release outranks any of its pre-releases.
        if a.prerelease.isEmpty || b.prerelease.isEmpty { return !a.prerelease.isEmpty && b.prerelease.isEmpty }
        for (x, y) in zip(a.prerelease, b.prerelease) where x != y {
            switch (Int(x), Int(y)) {
            case let (i?, j?): return i < j
            case (.some, nil): return true   // numeric identifiers sort before alphanumeric ones
            case (nil, .some): return false
            case (nil, nil): return x < y
            }
        }
        return a.prerelease.count < b.prerelease.count
    }
}
