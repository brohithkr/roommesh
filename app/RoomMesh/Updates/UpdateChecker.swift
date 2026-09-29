import AppKit
import Observation
import os

/// Checks GitHub Releases of brohithkr/roommesh for a newer RoomMesh, downloads its installer
/// package, verifies it against the release's SHA256SUMS.txt and opens it in Installer (whose
/// preinstall script quits RoomMesh and reopens it afterwards).
@MainActor @Observable
final class UpdateChecker {
    enum State: Equatable {
        case idle, checking, upToDate
        case available(AvailableUpdate)
        case downloading(Double)
        case readyToInstall(URL)
        case failed(String)
    }

    static let repository = "brohithkr/roommesh"
    static let latestURL = URL(string: "https://api.github.com/repos/\(repository)/releases/latest")!
    static let releasesURL = URL(string: "https://api.github.com/repos/\(repository)/releases?per_page=30")!
    static let releasesPage = URL(string: "https://github.com/\(repository)/releases")!
    nonisolated static let automaticInterval: TimeInterval = 24 * 60 * 60

    private(set) var state: State = .idle
    /// The newer release last found. Kept while it downloads or installs, and after a failed
    /// download so it can be retried; `nil` once a check finds nothing newer.
    private(set) var update: AvailableUpdate?
    /// The Developer ID the downloaded package is signed with (`nil`: unsigned, which is allowed).
    private(set) var packageSignature: String?
    let currentVersion: String
    let settings: SettingsStore

    @ObservationIgnored let http: any UpdateHTTP
    @ObservationIgnored let cacheDirectory: URL
    @ObservationIgnored var presentWindow: () -> Void = { UpdateWindowController.shared.show() }
    @ObservationIgnored var openPackage: (URL) -> Void = { NSWorkspace.shared.open($0) }
    @ObservationIgnored var signatureCheck: @Sendable (URL) -> String? = { PackageSignature.developerID(of: $0) }
    @ObservationIgnored var now: () -> Date = Date.init
    @ObservationIgnored private var automaticTask: Task<Void, Never>?
    @ObservationIgnored private var lastAttempt: Date?
    @ObservationIgnored private static let log = Logger(subsystem: Bundle.main.bundleIdentifier ?? "RoomMesh", category: "updates")

    init(settings: SettingsStore, currentVersion: String, http: any UpdateHTTP, cacheDirectory: URL) {
        self.settings = settings
        self.currentVersion = currentVersion
        self.http = http
        self.cacheDirectory = cacheDirectory
    }

    /// The running app's checker: its bundle version, URLSession, `~/Library/Caches/<bundle id>/Updates/`.
    static func live(settings: SettingsStore) -> UpdateChecker {
        let version = Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? "0.0.0"
        let caches = FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask)[0]
        let dir = caches.appending(path: Bundle.main.bundleIdentifier ?? "io.github.brohithkr.RoomMesh").appending(path: "Updates")
        return UpdateChecker(settings: settings, currentVersion: version, http: URLSessionUpdateHTTP(), cacheDirectory: dir)
    }

    // MARK: derived state
    var lastChecked: Date? { settings.lastUpdateCheck }
    /// The "X.Y.Z is available" status line: a background find the user hasn't put off with "Later".
    var bannerUpdate: AvailableUpdate? {
        guard case .available(let u) = state, settings.dismissedUpdateVersion != u.version.description else { return nil }
        return u
    }
    var isBusy: Bool {
        switch state { case .checking, .downloading: true; default: false }
    }

    private var headers: [String: String] {
        ["Accept": "application/vnd.github+json",
         "X-GitHub-Api-Version": "2022-11-28",
         "User-Agent": "RoomMesh/\(currentVersion) (macOS; +https://github.com/\(Self.repository))"]
    }
    private var downloadHeaders: [String: String] {
        ["Accept": "application/octet-stream", "User-Agent": headers["User-Agent"]!]
    }

    // MARK: checking
    /// Looks for a newer release. A user-initiated check shows the update window (with whatever it
    /// finds, including "up to date" or the error); a background one only updates the state, which
    /// the status banner and menu pick up. Ignored while a check or download is running.
    func checkForUpdates(userInitiated: Bool) async {
        if userInitiated { presentWindow() }
        switch state {
        case .checking, .downloading: return
        case .readyToInstall: return // keep the verified download; the window offers Install
        default: break
        }
        let previous = state
        state = .checking
        lastAttempt = now()
        do {
            let found = try await newestRelease()
            settings.lastUpdateCheck = now()
            let current = SemanticVersion(currentVersion) ?? SemanticVersion(0, 0, 0)
            if let found, found.version > current {
                Self.log.notice("update available: \(found.version.description, privacy: .public) (have \(self.currentVersion, privacy: .public))")
                update = found
                state = .available(found)
            } else {
                update = nil
                state = .upToDate
            }
        } catch {
            let message = Self.describe(error)
            Self.log.error("update check failed: \(message, privacy: .public)")
            // Offline the next day: keep offering what was already found.
            if !userInitiated, case .available = previous { state = previous } else { state = .failed(message) }
        }
    }

    private func newestRelease() async throws -> AvailableUpdate? {
        let decoder = JSONDecoder()
        if settings.includePrereleases {
            let (data, status) = try await http.get(Self.releasesURL, headers: headers)
            try Self.check(status)
            let releases = try decoder.decode([GitHubRelease].self, from: data)
            return ReleaseSelection.pick(releases, includePrereleases: true).flatMap(AvailableUpdate.init(release:))
        }
        let (data, status) = try await http.get(Self.latestURL, headers: headers)
        if status == 404 { return nil } // nothing published yet
        try Self.check(status)
        let release = try decoder.decode(GitHubRelease.self, from: data)
        return ReleaseSelection.pick([release], includePrereleases: false).flatMap(AvailableUpdate.init(release:))
    }

    private static func check(_ status: Int) throws {
        if status == 403 || status == 429 { throw UpdateError.rateLimited }
        guard (200..<300).contains(status) else { throw UpdateError.http(status) }
    }

    private static func describe(_ error: Error) -> String {
        switch error {
        case let e as UpdateError: e.localizedDescription
        case is DecodingError: "GitHub sent a reply RoomMesh couldn't read."
        default: error.localizedDescription
        }
    }

    // MARK: downloading and installing
    /// Downloads the release's installer package into the cache folder and verifies its SHA-256
    /// against SHA256SUMS.txt (fetched first, so a release without an entry isn't downloaded).
    func download() async {
        guard let update, !isBusy else { return }
        guard let package = update.package else { state = .failed(UpdateError.noPackage.localizedDescription); return }
        guard let checksums = update.checksums else { state = .failed(UpdateError.noChecksums.localizedDescription); return }
        state = .downloading(0)
        packageSignature = nil
        let destination = cacheDirectory.appending(path: package.name)
        do {
            let (data, status) = try await http.get(checksums.browserDownloadURL, headers: downloadHeaders)
            try Self.check(status)
            let sums = SHA256Sums.parse(String(decoding: data, as: UTF8.self))
            guard sums[package.name] != nil else { throw UpdateError.missingChecksum(package.name) }

            try FileManager.default.createDirectory(at: cacheDirectory, withIntermediateDirectories: true)
            removeOldDownloads()
            let target = WeakTarget(self)
            try await http.download(package.browserDownloadURL, headers: downloadHeaders, to: destination) { p in
                // The main queue keeps the reports in order (unlike one Task per report).
                DispatchQueue.main.async { MainActor.assumeIsolated { target.checker?.reportProgress(p) } }
            }
            do {
                try await Task.detached { try SHA256Sums.verify(file: destination, name: package.name, sums: sums) }.value
            } catch {
                try? FileManager.default.removeItem(at: destination)
                throw error
            }
            let check = signatureCheck
            let signature = await Task.detached { check(destination) }.value
            packageSignature = signature
            Self.log.notice("downloaded and verified \(package.name, privacy: .public); signature: \(signature ?? "none (unsigned)", privacy: .public)")
            state = .readyToInstall(destination)
        } catch {
            let message = Self.describe(error)
            Self.log.error("update download failed: \(message, privacy: .public)")
            state = .failed(message)
        }
    }

    /// The "Download & Install" button: download, verify, then open the installer.
    func downloadAndInstall() async {
        await download()
        if case .readyToInstall = state { install() }
    }

    /// Opens the verified package in Installer. Its preinstall quits RoomMesh.
    func install() {
        guard case .readyToInstall(let pkg) = state else { return }
        Self.log.notice("opening installer \(pkg.path, privacy: .public)")
        openPackage(pkg)
    }

    /// "Later": stop showing the status line for this version (the menu item stays).
    func later() {
        if let update { settings.dismissedUpdateVersion = update.version.description }
    }

    /// Download progress; ignored unless a download is running (reports arrive asynchronously).
    func reportProgress(_ fraction: Double) {
        guard case .downloading = state else { return }
        state = .downloading(min(max(fraction, 0), 1))
    }

    private func removeOldDownloads() {
        let fm = FileManager.default
        for file in (try? fm.contentsOfDirectory(at: cacheDirectory, includingPropertiesForKeys: nil)) ?? []
        where file.pathExtension.lowercased() == "pkg" {
            try? fm.removeItem(at: file)
        }
    }

    // MARK: automatic checks
    /// A background check, if "Check for updates automatically" is on.
    func automaticCheck() async {
        guard settings.checkForUpdatesAutomatically else { return }
        await checkForUpdates(userInitiated: false)
    }

    nonisolated static func isDue(lastAttempt: Date?, now: Date, interval: TimeInterval = automaticInterval) -> Bool {
        guard let lastAttempt else { return true }
        return now.timeIntervalSince(lastAttempt) >= interval
    }

    /// Once `launchDelay` after launch, then whenever 24 h have passed since the last attempt
    /// (looked at every `tick`, so a Mac that slept through the deadline checks soon after waking).
    func startAutomaticChecks(launchDelay: TimeInterval = 10, tick: TimeInterval = 60 * 60) {
        guard automaticTask == nil else { return }
        automaticTask = Task { [weak self] in
            try? await Task.sleep(for: .seconds(launchDelay))
            guard !Task.isCancelled else { return }
            await self?.automaticCheck()
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(tick))
                guard !Task.isCancelled, let self else { return }
                if Self.isDue(lastAttempt: self.lastAttempt, now: self.now()) { await self.automaticCheck() }
            }
        }
    }

    func stopAutomaticChecks() {
        automaticTask?.cancel()
        automaticTask = nil
    }
}

/// Lets the download's progress callback (any thread) reach the checker without keeping it alive.
private final class WeakTarget: @unchecked Sendable {
    weak var checker: UpdateChecker?
    init(_ checker: UpdateChecker) { self.checker = checker }
}
