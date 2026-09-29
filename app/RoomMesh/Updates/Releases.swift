import Foundation

/// The parts of a GitHub release (REST API) the updater uses.
struct GitHubRelease: Decodable, Equatable, Sendable {
    let tagName: String
    let name: String?
    let body: String?
    let htmlURL: URL
    let draft: Bool
    let prerelease: Bool
    let assets: [ReleaseAsset]

    enum CodingKeys: String, CodingKey {
        case tagName = "tag_name", name, body, htmlURL = "html_url", draft, prerelease, assets
    }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        tagName = try c.decode(String.self, forKey: .tagName)
        name = try c.decodeIfPresent(String.self, forKey: .name)
        body = try c.decodeIfPresent(String.self, forKey: .body)
        htmlURL = try c.decode(URL.self, forKey: .htmlURL)
        draft = try c.decodeIfPresent(Bool.self, forKey: .draft) ?? false
        prerelease = try c.decodeIfPresent(Bool.self, forKey: .prerelease) ?? false
        assets = try c.decodeIfPresent([ReleaseAsset].self, forKey: .assets) ?? []
    }
}

struct ReleaseAsset: Decodable, Equatable, Sendable {
    let name: String
    let browserDownloadURL: URL
    enum CodingKeys: String, CodingKey { case name, browserDownloadURL = "browser_download_url" }
    init(name: String, browserDownloadURL: URL) { (self.name, self.browserDownloadURL) = (name, browserDownloadURL) }

    /// A plain file name, safe to use inside the download folder.
    var hasSafeName: Bool {
        !name.isEmpty && !name.hasPrefix(".") && !name.contains("/") && !name.contains("\\") && !name.contains(":")
    }
}

/// A release newer than (or compared against) the running app, as the UI shows it.
struct AvailableUpdate: Equatable, Sendable {
    let version: SemanticVersion
    let tag: String
    let title: String
    /// Release notes as plain text (GitHub's Markdown `body`, shown as written).
    let notes: String
    let pageURL: URL
    let isPrerelease: Bool
    /// The guided installer (`RoomMesh-<X.Y.Z>.pkg`); it quits and reopens RoomMesh itself.
    let package: ReleaseAsset?
    /// `SHA256SUMS.txt`: `<sha256>  <filename>` lines.
    let checksums: ReleaseAsset?

    init?(release: GitHubRelease) {
        guard let version = SemanticVersion(release.tagName) else { return nil }
        self.version = version
        tag = release.tagName
        title = release.name.flatMap { $0.trimmingCharacters(in: .whitespaces).isEmpty ? nil : $0 } ?? "RoomMesh \(version)"
        notes = (release.body ?? "").replacingOccurrences(of: "\r\n", with: "\n").trimmingCharacters(in: .whitespacesAndNewlines)
        pageURL = release.htmlURL
        isPrerelease = release.prerelease || version.isPrerelease
        package = ReleaseAssets.package(in: release.assets, version: version)
        checksums = ReleaseAssets.checksums(in: release.assets)
    }
}

enum ReleaseSelection {
    /// The newest (highest-versioned) usable release: never a draft, a pre-release only if asked for.
    static func pick(_ releases: [GitHubRelease], includePrereleases: Bool) -> GitHubRelease? {
        releases
            .filter { !$0.draft && (includePrereleases || !$0.prerelease) }
            .compactMap { r in SemanticVersion(r.tagName).map { (r, $0) } }
            .filter { includePrereleases || !$0.1.isPrerelease }
            .max { $0.1 < $1.1 }?.0
    }
}

enum ReleaseAssets {
    static let checksumsName = "SHA256SUMS.txt"

    /// `RoomMesh-<version>.pkg`, else any other RoomMesh `.pkg` (never the `.dmg`).
    static func package(in assets: [ReleaseAsset], version: SemanticVersion) -> ReleaseAsset? {
        let safe = assets.filter(\.hasSafeName)
        if let exact = safe.first(where: { $0.name == "RoomMesh-\(version).pkg" }) { return exact }
        return safe.first { $0.name.lowercased().hasPrefix("roommesh") && $0.name.lowercased().hasSuffix(".pkg") }
    }
    static func checksums(in assets: [ReleaseAsset]) -> ReleaseAsset? {
        assets.first { $0.name.caseInsensitiveCompare(checksumsName) == .orderedSame }
    }
}
