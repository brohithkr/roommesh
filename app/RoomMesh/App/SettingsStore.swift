import Foundation
import Observation

extension UserDefaults {
    /// `ROOMMESH_PROFILE=b open -n RoomMesh.app` runs a second instance with its own identity (local testing).
    static var roomMesh: UserDefaults {
        if let p = ProcessInfo.processInfo.environment["ROOMMESH_PROFILE"], let d = UserDefaults(suiteName: "io.github.brohithkr.RoomMesh.\(p)") { return d }
        return .standard
    }
}

@MainActor @Observable
final class SettingsStore {
    @ObservationIgnored private let d: UserDefaults
    @ObservationIgnored var onChange: (() -> Void)?

    var inputDevice: String? { didSet { d.set(inputDevice, forKey: "inputDevice"); onChange?() } }
    var outputDevice: String? { didSet { d.set(outputDevice, forKey: "outputDevice"); onChange?() } }
    var allowSimultaneousTalkers: Bool { didSet { d.set(allowSimultaneousTalkers, forKey: "multiTalk"); onChange?() } }
    var echoCancellation: Bool { didSet { d.set(echoCancellation, forKey: "aec"); onChange?() } }
    var noiseSuppression: Bool { didSet { d.set(noiseSuppression, forKey: "ns"); onChange?() } }
    var micLatencyMs: UInt32 { didSet { d.set(Int(micLatencyMs), forKey: "micLatency"); onChange?() } }
    var playoutDelayMs: UInt32 { didSet { d.set(Int(playoutDelayMs), forKey: "playout"); onChange?() } }
    var autoElectCoordinator: Bool { didSet { d.set(autoElectCoordinator, forKey: "autoElect"); onChange?() } }
    var fallbackSpeakerToCoordinator: Bool { didSet { d.set(fallbackSpeakerToCoordinator, forKey: "fallbackSpeaker"); onChange?() } }
    var showWindowAtLaunch: Bool { didSet { d.set(showWindowAtLaunch, forKey: "showWindow") } }
    /// Look for a new release once at launch and then daily (`UpdateChecker`).
    var checkForUpdatesAutomatically: Bool { didSet { d.set(checkForUpdatesAutomatically, forKey: "autoUpdateCheck") } }
    /// Offer GitHub pre-releases too.
    var includePrereleases: Bool { didSet { d.set(includePrereleases, forKey: "updatePrereleases") } }
    /// The last successful update check.
    var lastUpdateCheck: Date? { didSet { d.set(lastUpdateCheck, forKey: "lastUpdateCheck") } }
    /// The version whose "available" status line the user put off with "Later".
    var dismissedUpdateVersion: String? { didSet { d.set(dismissedUpdateVersion, forKey: "dismissedUpdateVersion") } }
    /// This Mac's noise baseline preference in dBFS (after processing); `nil` = Automatic. A room
    /// this Mac creates starts with it, and the mic meter uses it outside a room. In a room, the
    /// room's own baseline applies (`AppModel.noiseBaselineDb`).
    var noiseBaselineDb: Double? {
        didSet {
            // "Custom" is stored explicitly, so a user who chose Automatic keeps it even if the
            // default changes.
            d.set(noiseBaselineDb != nil, forKey: "noiseBaselineCustom")
            if let noiseBaselineDb { d.set(noiseBaselineDb, forKey: "noiseBaseline") }
            onChange?()
        }
    }

    /// The noise baseline until the user picks one (`nil` = Automatic). The core has its own
    /// default (`DEFAULT_NOISE_BASELINE_DB` in core/roommesh-core/src/dsp/vad.rs); keep them the
    /// same. Real-room measurements will tune this later.
    static let defaultNoiseBaselineDb: Double? = nil

    init(defaults: UserDefaults) {
        d = defaults
        // micLatency 70 ms matches the core's default.
        d.register(defaults: ["aec": true, "ns": true, "micLatency": 70, "playout": 80, "autoElect": true, "showWindow": true,
                              "autoUpdateCheck": true])
        inputDevice = d.string(forKey: "inputDevice")
        outputDevice = d.string(forKey: "outputDevice")
        allowSimultaneousTalkers = d.bool(forKey: "multiTalk")
        echoCancellation = d.bool(forKey: "aec")
        noiseSuppression = d.bool(forKey: "ns")
        micLatencyMs = UInt32(d.integer(forKey: "micLatency"))
        playoutDelayMs = UInt32(d.integer(forKey: "playout"))
        autoElectCoordinator = d.bool(forKey: "autoElect")
        fallbackSpeakerToCoordinator = d.bool(forKey: "fallbackSpeaker")
        showWindowAtLaunch = d.bool(forKey: "showWindow")
        checkForUpdatesAutomatically = d.bool(forKey: "autoUpdateCheck")
        includePrereleases = d.bool(forKey: "updatePrereleases")
        lastUpdateCheck = d.object(forKey: "lastUpdateCheck") as? Date
        dismissedUpdateVersion = d.string(forKey: "dismissedUpdateVersion")
        if d.object(forKey: "noiseBaselineCustom") == nil {
            noiseBaselineDb = Self.defaultNoiseBaselineDb
        } else {
            noiseBaselineDb = d.bool(forKey: "noiseBaselineCustom") ? d.double(forKey: "noiseBaseline") : nil
        }
    }

    var ffi: FfiSettings {
        FfiSettings(inputDevice: inputDevice, outputDevice: outputDevice, allowSimultaneousTalkers: allowSimultaneousTalkers,
                    echoCancellation: echoCancellation, noiseSuppression: noiseSuppression, micLatencyMs: micLatencyMs,
                    playoutDelayMs: playoutDelayMs, autoElectCoordinator: autoElectCoordinator,
                    fallbackSpeakerToCoordinator: fallbackSpeakerToCoordinator,
                    noiseBaselineDb: noiseBaselineDb.map { Float($0) })
    }

    /// Peer ids are 16 lowercase hex characters (what `generatePeerId()` produces and the core accepts).
    nonisolated static func isValidPeerId(_ id: String) -> Bool {
        id.utf8.count == 16 && id.utf8.allSatisfy { (48...57).contains($0) || (97...102).contains($0) }
    }

    func peerId() -> String {
        if let id = d.string(forKey: "peerId"), Self.isValidPeerId(id) { return id }
        let id = generatePeerId()
        d.set(id, forKey: "peerId")
        return id
    }
}
