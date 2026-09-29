import CryptoKit
import XCTest
@testable import RoomMesh

/// Canned GitHub responses; the update tests never touch the network.
final class FakeHTTP: UpdateHTTP, @unchecked Sendable {
    private let lock = NSLock()
    private var _responses: [String: (Data, Int)] = [:]
    private var _downloads: [String: Result<Data, Error>] = [:]
    private var _requests: [(url: URL, headers: [String: String])] = []

    var requests: [(url: URL, headers: [String: String])] { lock.withLock { _requests } }
    var requestedURLs: [String] { requests.map(\.url.absoluteString) }
    func respond(_ url: URL, _ body: Data, status: Int = 200) { lock.withLock { _responses[url.absoluteString] = (body, status) } }
    func respond(_ url: String, _ body: String, status: Int = 200) { respond(URL(string: url)!, Data(body.utf8), status: status) }
    func serve(_ url: String, _ result: Result<Data, Error>) { lock.withLock { _downloads[url] = result } }

    func get(_ url: URL, headers: [String: String]) async throws -> (Data, Int) {
        let r = lock.withLock { _requests.append((url, headers)); return _responses[url.absoluteString] }
        guard let r else { throw URLError(.cannotFindHost) }
        return r
    }
    func download(_ url: URL, headers: [String: String], to destination: URL, progress: @escaping @Sendable (Double) -> Void) async throws {
        let r = lock.withLock { _requests.append((url, headers)); return _downloads[url.absoluteString] }
        progress(0.5)
        switch r {
        case .success(let data)?: try data.write(to: destination); progress(1)
        case .failure(let error)?: throw error
        case nil: throw URLError(.fileDoesNotExist)
        }
    }
}

// MARK: fixtures
private let dl = "https://github.com/brohithkr/roommesh/releases/download"
private func asset(_ name: String, tag: String) -> [String: Any] {
    ["name": name, "browser_download_url": "\(dl)/\(tag)/\(name)", "size": 10]
}
private func releaseJSON(_ version: String, draft: Bool = false, prerelease: Bool = false,
                         assets: [String]? = nil, body: String = "- Fixed things") -> [String: Any] {
    let tag = "v\(version)"
    let names = assets ?? ["RoomMesh-\(version).pkg", "RoomMesh-\(version).dmg", "SHA256SUMS.txt"]
    return ["tag_name": tag, "name": "RoomMesh \(version)", "body": body, "draft": draft, "prerelease": prerelease,
            "html_url": "https://github.com/brohithkr/roommesh/releases/tag/\(tag)",
            "assets": names.map { asset($0, tag: tag) }]
}
private func json(_ object: Any) -> Data { try! JSONSerialization.data(withJSONObject: object) }
private func sha256(_ data: Data) -> String { SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined() }

@MainActor
final class UpdateCheckerTests: XCTestCase {
    var http: FakeHTTP!
    var settings: SettingsStore!
    var checker: UpdateChecker!
    var cache: URL!
    var windows = 0
    var opened: [URL] = []
    private let suite = "test.\(UUID())"

    override func setUp() async throws {
        http = FakeHTTP()
        settings = SettingsStore(defaults: UserDefaults(suiteName: suite)!)
        cache = FileManager.default.temporaryDirectory.appending(path: "roommesh-updates-\(UUID())")
        checker = UpdateChecker(settings: settings, currentVersion: "1.0.0", http: http, cacheDirectory: cache)
        windows = 0; opened = []
        checker.presentWindow = { [unowned self] in windows += 1 }
        checker.openPackage = { [unowned self] in opened.append($0) }
        checker.signatureCheck = { _ in nil }
    }
    override func tearDown() async throws {
        checker.stopAutomaticChecks()
        UserDefaults.standard.removePersistentDomain(forName: suite)
        try? FileManager.default.removeItem(at: cache)
    }

    // MARK: versions
    func testSemanticVersionComparison() throws {
        func v(_ s: String) throws -> SemanticVersion { try XCTUnwrap(SemanticVersion(s), s) }
        XCTAssertGreaterThan(try v("1.10.0"), try v("1.9.0"), "numeric, not string, comparison")
        XCTAssertEqual(try v("v1.2.3"), try v("1.2.3"), "a v prefix is ignored")
        XCTAssertEqual(try v("V2.0.0"), try v("2.0.0"))
        XCTAssertGreaterThan(try v("2.0.0"), try v("1.99.99"))
        XCTAssertLessThan(try v("1.0.0"), try v("1.0.1"))
        XCTAssertEqual(try v("1.2"), try v("1.2.0"))
        XCTAssertLessThan(try v("1.0.0-beta.1"), try v("1.0.0"), "a pre-release sorts before its release")
        XCTAssertLessThan(try v("1.0.0-beta.2"), try v("1.0.0-beta.10"))
        XCTAssertLessThan(try v("1.0.0-alpha"), try v("1.0.0-beta"))
        XCTAssertLessThan(try v("1.0.0-1"), try v("1.0.0-alpha"), "numeric identifiers sort first")
        XCTAssertEqual(try v("1.0.0+build.5"), try v("1.0.0"), "build metadata is ignored")
        XCTAssertEqual(try v("v1.10.0").description, "1.10.0")
        XCTAssertEqual(try v("1.1.0-rc.1").description, "1.1.0-rc.1")
        for bad in ["", "abc", "1.x.0", "1.2.3.4", "v", "1..2", "-1.0.0"] { XCTAssertNil(SemanticVersion(bad), bad) }
    }

    // MARK: releases and assets
    func testDecodesARelease() throws {
        let r = try JSONDecoder().decode(GitHubRelease.self, from: json(releaseJSON("1.2.0")))
        XCTAssertEqual(r.tagName, "v1.2.0")
        XCTAssertEqual(r.name, "RoomMesh 1.2.0")
        XCTAssertEqual(r.body, "- Fixed things")
        XCTAssertEqual(r.htmlURL.absoluteString, "https://github.com/brohithkr/roommesh/releases/tag/v1.2.0")
        XCTAssertEqual(r.assets.map(\.name), ["RoomMesh-1.2.0.pkg", "RoomMesh-1.2.0.dmg", "SHA256SUMS.txt"])
        XCTAssertEqual(r.assets[0].browserDownloadURL.absoluteString, "\(dl)/v1.2.0/RoomMesh-1.2.0.pkg")
        let update = try XCTUnwrap(AvailableUpdate(release: r))
        XCTAssertEqual(update.version.description, "1.2.0")
        XCTAssertEqual(update.package?.name, "RoomMesh-1.2.0.pkg")
        XCTAssertEqual(update.checksums?.name, "SHA256SUMS.txt")
    }
    func testDecodesAReleaseWithNullNameAndBody() throws {
        var raw = releaseJSON("1.2.0")
        raw["name"] = NSNull(); raw["body"] = NSNull()
        let r = try JSONDecoder().decode(GitHubRelease.self, from: json(raw))
        XCTAssertEqual(AvailableUpdate(release: r)?.title, "RoomMesh 1.2.0")
        XCTAssertEqual(AvailableUpdate(release: r)?.notes, "")
    }
    func testPicksTheInstallerPackage() throws {
        func pick(_ names: [String], _ version: String = "1.2.0") -> String? {
            let assets = names.map { ReleaseAsset(name: $0, browserDownloadURL: URL(string: "\(dl)/v1/\($0)")!) }
            return ReleaseAssets.package(in: assets, version: SemanticVersion(version)!)?.name
        }
        XCTAssertEqual(pick(["RoomMesh-1.2.0.dmg", "RoomMesh-debug.pkg", "RoomMesh-1.2.0.pkg", "SHA256SUMS.txt"]), "RoomMesh-1.2.0.pkg")
        XCTAssertEqual(pick(["RoomMesh-1.2.0.dmg", "RoomMesh-Installer.pkg"]), "RoomMesh-Installer.pkg", "falls back to any RoomMesh pkg")
        XCTAssertNil(pick(["RoomMesh-1.2.0.dmg", "SHA256SUMS.txt"]), "never the dmg")
        XCTAssertNil(pick(["../evil.pkg"]), "no path components")
    }
    func testPrereleaseFiltering() throws {
        let releases = try JSONDecoder().decode([GitHubRelease].self, from: json([
            releaseJSON("2.0.0", draft: true),
            releaseJSON("1.5.0-beta.1", prerelease: true),
            releaseJSON("1.4.0"),
            releaseJSON("1.3.0"),
        ]))
        XCTAssertEqual(ReleaseSelection.pick(releases, includePrereleases: false)?.tagName, "v1.4.0", "drafts and pre-releases skipped")
        XCTAssertEqual(ReleaseSelection.pick(releases, includePrereleases: true)?.tagName, "v1.5.0-beta.1", "drafts still skipped")
        XCTAssertNil(ReleaseSelection.pick(Array(releases.prefix(1)), includePrereleases: true))
    }

    // MARK: checksums
    func testParsesSHA256Sums() {
        let a = String(repeating: "a", count: 64), b = String(repeating: "B", count: 64)
        let sums = SHA256Sums.parse("\(a)  RoomMesh-1.2.0.pkg\n\n\(b) *RoomMesh-1.2.0.dmg\r\n# comment\nnot a line\n")
        XCTAssertEqual(sums["RoomMesh-1.2.0.pkg"], a)
        XCTAssertEqual(sums["RoomMesh-1.2.0.dmg"], b.lowercased(), "binary marker stripped, hex lower-cased")
        XCTAssertEqual(sums.count, 2)
        XCTAssertNil(sums["RoomMesh-1.3.0.pkg"])
    }
    func testVerifiesAFileChecksum() throws {
        try FileManager.default.createDirectory(at: cache, withIntermediateDirectories: true)
        let file = cache.appending(path: "RoomMesh-1.2.0.pkg")
        try Data("hello".utf8).write(to: file)
        let good = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        XCTAssertEqual(try SHA256Sums.hash(of: file), good)
        XCTAssertNoThrow(try SHA256Sums.verify(file: file, name: "RoomMesh-1.2.0.pkg", sums: ["RoomMesh-1.2.0.pkg": good]))
        XCTAssertThrowsError(try SHA256Sums.verify(file: file, name: "RoomMesh-1.2.0.pkg", sums: ["RoomMesh-1.2.0.pkg": String(repeating: "0", count: 64)])) {
            XCTAssertEqual($0 as? UpdateError, .checksumMismatch("RoomMesh-1.2.0.pkg"))
        }
        XCTAssertThrowsError(try SHA256Sums.verify(file: file, name: "RoomMesh-1.2.0.pkg", sums: [:])) {
            XCTAssertEqual($0 as? UpdateError, .missingChecksum("RoomMesh-1.2.0.pkg"))
        }
    }
    func testReadsTheDeveloperIDFromPkgutil() {
        let signed = """
        Package "RoomMesh-1.2.0.pkg":
           Status: signed by a developer certificate issued by Apple for distribution
           Notarization: trusted by the Apple notary service
           Certificate Chain:
            1. Developer ID Installer: Rohith B (ABCDE12345)
               Expires: 2030-01-01 00:00:00 +0000
            2. Developer ID Certification Authority
        """
        XCTAssertEqual(PackageSignature.developerID(fromPkgutilOutput: signed), "Developer ID Installer: Rohith B (ABCDE12345)")
        XCTAssertNil(PackageSignature.developerID(fromPkgutilOutput: "Package \"x.pkg\":\n   Status: no signature\n"))
    }

    // MARK: state machine
    func testUpToDate() async {
        http.respond(UpdateChecker.latestURL, json(releaseJSON("1.0.0")))
        await checker.checkForUpdates(userInitiated: false)
        XCTAssertEqual(checker.state, .upToDate)
        XCTAssertNil(checker.update)
        XCTAssertNotNil(checker.lastChecked)
        XCTAssertEqual(windows, 0)
    }
    func testOlderReleaseIsUpToDate() async {
        http.respond(UpdateChecker.latestURL, json(releaseJSON("0.9.9")))
        await checker.checkForUpdates(userInitiated: false)
        XCTAssertEqual(checker.state, .upToDate)
    }
    func testNoReleasesYetIsUpToDate() async {
        http.respond(UpdateChecker.latestURL.absoluteString, #"{"message":"Not Found"}"#, status: 404)
        await checker.checkForUpdates(userInitiated: true)
        XCTAssertEqual(checker.state, .upToDate)
    }
    func testRequestsSendGitHubHeaders() async throws {
        http.respond(UpdateChecker.latestURL, json(releaseJSON("1.0.0")))
        await checker.checkForUpdates(userInitiated: false)
        let headers = try XCTUnwrap(http.requests.first?.headers)
        XCTAssertEqual(headers["Accept"], "application/vnd.github+json")
        XCTAssertEqual(headers["User-Agent"]?.hasPrefix("RoomMesh/1.0.0"), true)
    }
    func testBackgroundFindShowsABannerNotAWindow() async throws {
        http.respond(UpdateChecker.latestURL, json(releaseJSON("1.10.0")))
        await checker.checkForUpdates(userInitiated: false)
        guard case .available(let u) = checker.state else { return XCTFail("\(checker.state)") }
        XCTAssertEqual(u.version.description, "1.10.0")
        XCTAssertEqual(u.pageURL.absoluteString, "https://github.com/brohithkr/roommesh/releases/tag/v1.10.0")
        XCTAssertEqual(windows, 0, "no modal for a background find")
        XCTAssertEqual(checker.bannerUpdate?.version.description, "1.10.0")
        XCTAssertEqual(checker.update?.version.description, "1.10.0", "drives the menu item")
        checker.later()
        XCTAssertNil(checker.bannerUpdate, "Later hides the banner for this version")
        XCTAssertNotNil(checker.update, "the menu item stays")
        let again = UpdateChecker(settings: SettingsStore(defaults: UserDefaults(suiteName: suite)!), currentVersion: "1.0.0", http: http, cacheDirectory: cache)
        await again.checkForUpdates(userInitiated: false)
        XCTAssertNil(again.bannerUpdate, "not shown again the next day")
    }
    func testUserInitiatedCheckOpensTheWindow() async {
        http.respond(UpdateChecker.latestURL, json(releaseJSON("1.1.0")))
        await checker.checkForUpdates(userInitiated: true)
        XCTAssertEqual(windows, 1)
        guard case .available = checker.state else { return XCTFail("\(checker.state)") }
    }
    func testServerErrorFails() async {
        http.respond(UpdateChecker.latestURL.absoluteString, "oops", status: 500)
        await checker.checkForUpdates(userInitiated: true)
        guard case .failed(let m) = checker.state else { return XCTFail("\(checker.state)") }
        XCTAssertTrue(m.contains("500"), m)
        XCTAssertNil(checker.lastChecked, "only successful checks count")
    }
    func testBackgroundFailureKeepsAKnownUpdate() async {
        http.respond(UpdateChecker.latestURL, json(releaseJSON("1.1.0")))
        await checker.checkForUpdates(userInitiated: false)
        http.respond(UpdateChecker.latestURL.absoluteString, "", status: 502)
        await checker.checkForUpdates(userInitiated: false)
        guard case .available = checker.state else { return XCTFail("offline the next day: keep offering the update") }
    }
    func testPrereleaseSettingListsAllReleases() async {
        http.respond(UpdateChecker.latestURL, json(releaseJSON("1.1.0")))
        http.respond(UpdateChecker.releasesURL, json([releaseJSON("1.2.0-beta.1", prerelease: true), releaseJSON("1.1.0")]))
        settings.includePrereleases = true
        await checker.checkForUpdates(userInitiated: false)
        XCTAssertEqual(http.requestedURLs, [UpdateChecker.releasesURL.absoluteString])
        XCTAssertEqual(checker.update?.version.description, "1.2.0-beta.1")
        settings.includePrereleases = false
        await checker.checkForUpdates(userInitiated: false)
        XCTAssertEqual(http.requestedURLs.last, UpdateChecker.latestURL.absoluteString)
        XCTAssertEqual(checker.update?.version.description, "1.1.0")
    }
    func testLatestEndpointPrereleaseIsIgnoredWhenOff() async {
        http.respond(UpdateChecker.latestURL, json(releaseJSON("1.1.0-rc.1", prerelease: true)))
        await checker.checkForUpdates(userInitiated: false)
        XCTAssertEqual(checker.state, .upToDate)
    }

    // MARK: download
    private func offer(_ pkg: Data, sums: String? = nil) async {
        http.respond(UpdateChecker.latestURL, json(releaseJSON("1.1.0")))
        http.respond("\(dl)/v1.1.0/SHA256SUMS.txt", sums ?? "\(sha256(pkg))  RoomMesh-1.1.0.pkg\n\(String(repeating: "c", count: 64))  RoomMesh-1.1.0.dmg\n")
        http.serve("\(dl)/v1.1.0/RoomMesh-1.1.0.pkg", .success(pkg))
        await checker.checkForUpdates(userInitiated: true)
    }
    func testDownloadVerifiesAndInstalls() async throws {
        let pkg = Data("installer bytes".utf8)
        await offer(pkg)
        checker.signatureCheck = { _ in "Developer ID Installer: Rohith B (ABCDE12345)" }
        await checker.downloadAndInstall()
        let expected = cache.appending(path: "RoomMesh-1.1.0.pkg")
        XCTAssertEqual(checker.state, .readyToInstall(expected))
        XCTAssertEqual(try Data(contentsOf: expected), pkg)
        XCTAssertEqual(opened, [expected], "opens the Installer wizard")
        XCTAssertEqual(checker.packageSignature, "Developer ID Installer: Rohith B (ABCDE12345)")
    }
    func testUnsignedPackageIsAllowed() async {
        await offer(Data("unsigned".utf8))
        await checker.download()
        guard case .readyToInstall = checker.state else { return XCTFail("\(checker.state)") }
        XCTAssertNil(checker.packageSignature)
    }
    func testDownloadFailure() async {
        await offer(Data("x".utf8))
        http.serve("\(dl)/v1.1.0/RoomMesh-1.1.0.pkg", .failure(URLError(.networkConnectionLost)))
        await checker.downloadAndInstall()
        guard case .failed = checker.state else { return XCTFail("\(checker.state)") }
        XCTAssertEqual(opened, [])
        XCTAssertNotNil(checker.update, "the release stays on offer so the user can retry")
    }
    func testChecksumMismatchIsRefused() async {
        let pkg = Data("tampered".utf8)
        await offer(pkg, sums: "\(String(repeating: "0", count: 64))  RoomMesh-1.1.0.pkg\n")
        await checker.downloadAndInstall()
        guard case .failed(let m) = checker.state else { return XCTFail("\(checker.state)") }
        XCTAssertTrue(m.localizedCaseInsensitiveContains("checksum"), m)
        XCTAssertEqual(opened, [])
        XCTAssertFalse(FileManager.default.fileExists(atPath: cache.appending(path: "RoomMesh-1.1.0.pkg").path), "bad download deleted")
    }
    func testMissingChecksumEntryIsRefusedBeforeDownloading() async {
        await offer(Data("x".utf8), sums: "\(String(repeating: "c", count: 64))  RoomMesh-1.1.0.dmg\n")
        await checker.downloadAndInstall()
        guard case .failed(let m) = checker.state else { return XCTFail("\(checker.state)") }
        XCTAssertTrue(m.contains("RoomMesh-1.1.0.pkg"), m)
        XCTAssertFalse(http.requestedURLs.contains("\(dl)/v1.1.0/RoomMesh-1.1.0.pkg"), "pkg not downloaded")
        XCTAssertEqual(opened, [])
    }
    func testReleaseWithoutAPackageCannotBeDownloaded() async {
        http.respond(UpdateChecker.latestURL, json(releaseJSON("1.1.0", assets: ["RoomMesh-1.1.0.dmg", "SHA256SUMS.txt"])))
        await checker.checkForUpdates(userInitiated: true)
        await checker.download()
        guard case .failed = checker.state else { return XCTFail("\(checker.state)") }
    }
    func testProgressOnlyAppliesWhileDownloading() async {
        checker.reportProgress(0.5)
        XCTAssertEqual(checker.state, .idle, "a late progress report doesn't resurrect a download")
    }
    func testInstallOnlyWhenReady() {
        checker.install()
        XCTAssertEqual(opened, [])
    }

    // MARK: automatic checks
    func testAutomaticCheckRespectsTheSetting() async {
        http.respond(UpdateChecker.latestURL, json(releaseJSON("1.0.0")))
        XCTAssertTrue(settings.checkForUpdatesAutomatically, "on by default")
        settings.checkForUpdatesAutomatically = false
        await checker.automaticCheck()
        XCTAssertEqual(http.requests.count, 0)
        XCTAssertEqual(checker.state, .idle)
        settings.checkForUpdatesAutomatically = true
        await checker.automaticCheck()
        XCTAssertEqual(http.requests.count, 1)
        XCTAssertEqual(checker.state, .upToDate)
    }
    func testAutomaticCheckIsDueEvery24Hours() {
        let t = Date(timeIntervalSince1970: 1_000_000)
        XCTAssertTrue(UpdateChecker.isDue(lastAttempt: nil, now: t))
        XCTAssertFalse(UpdateChecker.isDue(lastAttempt: t, now: t.addingTimeInterval(23 * 3600)))
        XCTAssertTrue(UpdateChecker.isDue(lastAttempt: t, now: t.addingTimeInterval(24 * 3600)))
    }
    func testScheduledChecksRunAfterTheLaunchDelay() async throws {
        http.respond(UpdateChecker.latestURL, json(releaseJSON("1.0.0")))
        checker.startAutomaticChecks(launchDelay: 0.05, tick: 60)
        XCTAssertEqual(http.requests.count, 0, "not at launch itself")
        for _ in 0..<100 where http.requests.isEmpty { try await Task.sleep(for: .milliseconds(20)) }
        XCTAssertEqual(http.requests.count, 1)
    }
    func testScheduledChecksSkippedWhenOff() async throws {
        settings.checkForUpdatesAutomatically = false
        checker.startAutomaticChecks(launchDelay: 0.01, tick: 0.01)
        try await Task.sleep(for: .milliseconds(200))
        XCTAssertEqual(http.requests.count, 0)
    }
}
