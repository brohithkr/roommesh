import AppKit
import Observation

struct PendingInvite: Identifiable, Equatable {
    let id: String          // room id
    let roomName: String
    let fromPeer: String
    let fromName: String
    let sas: String
}

enum RoomHealth: Equatable { case excellent, good, degraded
    var label: String { switch self { case .excellent: "Excellent"; case .good: "Good"; case .degraded: "Degraded" } }
}
enum MenuIconState: Equatable { case notConnected, connected, muted, warning }
enum SettingsTab: Hashable { case general, audio, network, advanced }

enum ActiveSheet: Identifiable, Equatable {
    case incomingInvite(PendingInvite), coordinatorLost([String]), speakerLost([String]), invite
    var id: String {
        switch self {
        case .incomingInvite(let i): "in-\(i.id)"
        case .coordinatorLost: "coordinator-lost"
        case .speakerLost: "speaker-lost"
        case .invite: "invite"
        }
    }
}

@MainActor @Observable
final class AppModel {
    static let shared = AppModel()

    let settings: SettingsStore
    /// Check for Updates (GitHub Releases).
    let updates: UpdateChecker
    @ObservationIgnored private(set) var core: (any RoomMeshCoreProtocol)?
    private(set) var localPeerId = ""
    /// Keeps App Nap off while this Mac carries audio (see `carryingAudio`).
    @ObservationIgnored let audioActivity: AudioActivity
    /// Settings' noise meter is running (it captures the mic even outside a room).
    @ObservationIgnored private(set) var meterRunning = false

    var room: FfiRoomState? { didSet { updateAudioActivity() } }
    var nearby: [FfiNearbyPeer] = []
    var incomingInvite: PendingInvite? { didSet { updateWindowFloating() } }
    var coordinatorLostCandidates: [String]? { didSet { updateWindowFloating() } }
    var speakerLostCandidates: [String]? { didSet { updateWindowFloating() } }
    /// Whether macOS will show RoomMesh's banners; `nil` until Notification Center's settings are
    /// read. When not `true`, invites and prompts use the in-app fallback (see `AttentionPlan`).
    /// Set from `Notifications.onStatusChange` (and directly by tests).
    var notificationsAllowed: Bool? { didSet { updateWindowFloating() } }
    /// Human-readable notification status for Settings › General.
    var notificationStatus: String?
    var showInviteSheet = false
    var lastError: String?
    /// Transient, non-error status line (auto-clears); see `showNotice`.
    var notice: String?
    var isMuted = false
    var aecConverged = false
    var qualities: [String: FfiQuality] = [:]
    var driverInstalled = false
    var virtualDeviceAvailable = false
    /// Cached by `refreshStatus` (the CoreAudio / hash / TCC checks are too costly per render).
    var needsDriverInstall = false
    var installedDriverVersion: String?
    var micPermission: MicPermission = .undetermined
    /// Non-nil while an install/uninstall runs behind the admin prompt.
    var driverOperation: DriverOperation?
    var localNetworkDenied = false
    var settingsTab: SettingsTab = .general

    @ObservationIgnored var presentWindow: () -> Void = { MainWindowController.shared.show() }
    @ObservationIgnored var notifyInvite: (String, String) -> Void = { Notifications.shared.inviteReceived(roomName: $0, from: $1) }
    /// Removes the delivered invite notification once the invite is answered.
    @ObservationIgnored var inviteAnswered: () -> Void = { Notifications.shared.inviteAnswered() }
    /// True when the invite sheet will be on screen anyway (app active, main window up): no banner
    /// (or fallback sound) then. Also used for the coordinator/speaker prompts.
    @ObservationIgnored var inviteUIVisible: () -> Bool = { NSApp.isActive && MainWindowController.shared.isVisible }
    /// Blocking privileged driver work, run off the main thread. Returns false if the user cancelled.
    @ObservationIgnored var driverWork: @Sendable (DriverOperation) throws -> Bool = { try DriverInstaller.perform($0) }
    /// Asks before restarting coreaudiod while in a room (it interrupts the room's audio).
    @ObservationIgnored var confirmAudioRestart: () -> Bool = {
        let alert = NSAlert()
        alert.messageText = "Restart audio on this Mac?"
        alert.informativeText = "Changing the RoomMesh driver restarts macOS audio, which briefly interrupts the room's audio and any meeting on this Mac."
        alert.addButton(withTitle: "Continue")
        alert.addButton(withTitle: "Cancel")
        return alert.runModal() == .alertFirstButtonReturn
    }
    /// The fallback's short sound when notifications are off.
    @ObservationIgnored var playAttentionSound: () -> Void = { NSSound(named: "Glass")?.play() }
    /// Floats the main window above other apps while a prompt waits and notifications are off.
    @ObservationIgnored var setWindowFloating: (Bool) -> Void = { MainWindowController.shared.setFloating($0) }
    @ObservationIgnored private var windowFloating = false
    @ObservationIgnored var devicesPresent: () -> Bool = { VirtualDeviceStatus.installed }
    @ObservationIgnored private var transport: AppleP2PTransport?
    @ObservationIgnored private var sink: CoreSink?
    @ObservationIgnored private var noticeGeneration = 0
    /// Id of the sheet SwiftUI last presented (see `sheetDidPresent`).
    @ObservationIgnored private(set) var presentedSheetID: String?

    /// `nil` → the app's persisted settings / the live update checker / the system's App Nap
    /// activity. (A `SettingsStore(...)` default argument would be evaluated outside the main actor.)
    init(settings: SettingsStore? = nil, updates: UpdateChecker? = nil, audioActivity: AudioActivity? = nil) {
        let settings = settings ?? SettingsStore(defaults: .roomMesh)
        self.settings = settings
        self.updates = updates ?? UpdateChecker.live(settings: settings)
        self.audioActivity = audioActivity ?? AudioActivity()
    }

    func bootstrap(driverInstalled: Bool) {
        guard core == nil else { return }
        self.driverInstalled = driverInstalled
        let transport = AppleP2PTransport()
        do {
            let core = try RoomMeshCore(peerId: settings.peerId(), name: Host.current().localizedName ?? "Mac",
                                        driverInstalled: driverInstalled, transport: transport,
                                        listener: EventRelay(model: self), settings: settings.ffi)
            let sink = CoreSink(core: core)
            transport.sink = sink
            observe(transport: transport)
            self.transport = transport
            self.sink = sink
            attach(core: core)
            core.start()
            core.startAudioEngine()
        } catch {
            lastError = "RoomMesh could not start: \(describe(error))"
        }
    }

    /// Test seam and second half of `bootstrap`.
    func attach(core: any RoomMeshCoreProtocol) {
        self.core = core
        localPeerId = core.localPeerId()
        settings.onChange = { [weak self] in guard let self else { return }; self.core?.updateSettings(settings: self.settings.ffi) }
    }

    /// Local-network permission signals from the transport (delivered off-main).
    func observe(transport: AppleP2PTransport) {
        transport.onLocalNetworkDenied = { [weak self] in
            DispatchQueue.main.async { [weak self] in MainActor.assumeIsolated { self?.localNetworkDenied = true } }
        }
        transport.onLocalNetworkAllowed = { [weak self] in
            DispatchQueue.main.async { [weak self] in MainActor.assumeIsolated { self?.localNetworkDenied = false } }
        }
    }

    func shutdown() {
        if inRoom { try? core?.leaveRoom() }
        core?.stopAudioEngine()
        core?.stop()
        audioActivity.update(carryingAudio: false)
    }

    // MARK: derived state
    var inRoom: Bool { room != nil }
    /// This Mac's audio is running: in a room, or the noise meter is on.
    var carryingAudio: Bool { inRoom || meterRunning }
    private func updateAudioActivity() { audioActivity.update(carryingAudio: carryingAudio) }
    func name(of id: String?) -> String? {
        guard let id else { return nil }
        return room?.members.first { $0.id == id }?.name ?? nearby.first { $0.id == id }?.name
    }
    var coordinatorName: String? { name(of: room?.coordinator) }
    var speakerName: String? { name(of: room?.speaker) }
    var activeMicName: String? { name(of: room?.activePrimary) }
    var secondaryMicName: String? { name(of: room?.activeSecondary) }
    var connectedCount: Int { room?.members.filter(\.online).count ?? 0 }
    var isLocalCoordinator: Bool { room.map { $0.coordinator == localPeerId } ?? false }
    var localMember: FfiMember? { room?.members.first { $0.isLocal } }
    var useMyMic: Bool { localMember?.micEnabled ?? true }
    var defaultRoomName: String { "\(Host.current().localizedName ?? "Mac")'s Room" }
    var invitableNearby: [FfiNearbyPeer] { nearby.filter { !$0.inMyRoom } }
    /// An invite or failure prompt is waiting for an answer.
    var hasAttentionPrompt: Bool { incomingInvite != nil || coordinatorLostCandidates != nil || speakerLostCandidates != nil }
    /// A dot on the menu-bar icon until the prompt is answered — only when banners can't do the job.
    var showsAttentionBadge: Bool { notificationsAllowed != true && hasAttentionPrompt }
    /// Only once the settings were actually read, so the banner doesn't flash at launch.
    var showsNotificationsOffBanner: Bool { notificationsAllowed == false }

    var health: RoomHealth {
        let ids = Set(room?.members.filter(\.online).map(\.id) ?? [])
        let qs = qualities.filter { ids.contains($0.key) }.values
        if qs.contains(.degraded) || qs.contains(.disconnected) { return .degraded }
        if qs.contains(.good) { return .good }
        return .excellent
    }
    var iconState: MenuIconState {
        guard inRoom else { return .notConnected }
        if coordinatorLostCandidates != nil || speakerLostCandidates != nil || health == .degraded
            || (isLocalCoordinator && driverInstalled && !virtualDeviceAvailable) { return .warning }
        return isMuted ? .muted : .connected
    }

    var activeSheet: ActiveSheet? {
        get {
            if let i = incomingInvite { return .incomingInvite(i) }
            if let c = coordinatorLostCandidates { return .coordinatorLost(c) }
            if let s = speakerLostCandidates { return .speakerLost(s) }
            return showInviteSheet ? .invite : nil
        }
        set {
            // SwiftUI writes nil when the user (or `dismiss()`) closes the presented sheet. The write
            // carries no identity, so act only if the presented sheet is still the one the model would
            // show; otherwise the state changed underneath (a new invite, a queued prompt) and the
            // write is stale.
            guard newValue == nil, let current = activeSheet, current.id == presentedSheetID else { return }
            dismiss(current)
        }
    }

    /// Called by the sheet content when it appears, so a later nil write can be matched to it.
    func sheetDidPresent(_ sheet: ActiveSheet) { presentedSheetID = sheet.id }
    /// Called from the sheet content's onDisappear; a late call for an older sheet is ignored.
    func sheetDidDisappear(_ sheet: ActiveSheet) { if presentedSheetID == sheet.id { presentedSheetID = nil } }

    /// Closes exactly `sheet` (closing an invite declines it); other pending sheets are untouched.
    func dismiss(_ sheet: ActiveSheet) {
        if presentedSheetID == sheet.id { presentedSheetID = nil }
        switch sheet {
        case .incomingInvite(let i): if incomingInvite?.id == i.id { respondToInvite(accept: false) }
        case .coordinatorLost: coordinatorLostCandidates = nil
        case .speakerLost: speakerLostCandidates = nil
        case .invite: showInviteSheet = false
        }
    }

    // MARK: attention
    /// Notifies (or falls back to a sound) and brings the window to the front for a new prompt.
    private func getAttention(_ kind: AttentionKind, invite: (roomName: String, from: String)? = nil) {
        let plan = AttentionPlan.make(for: kind, notificationsAllowed: notificationsAllowed, uiVisible: inviteUIVisible())
        if plan.postNotification, let invite { notifyInvite(invite.roomName, invite.from) }
        if plan.playSound { playAttentionSound() }
        if plan.bringToFront { presentWindow() }
    }

    /// Keeps the main window floating exactly while `showsAttentionBadge` holds (prompt waiting,
    /// banners unavailable); the controller restores the window's own level afterwards.
    private func updateWindowFloating() {
        let want = showsAttentionBadge
        guard want != windowFloating else { return }
        windowFloating = want
        setWindowFloating(want)
    }

    // MARK: actions
    private func run(_ body: (any RoomMeshCoreProtocol) throws -> Void) {
        guard let core else { return }
        do { try body(core) } catch { lastError = describe(error) }
    }
    func createRoom(name: String) { run { try $0.createRoom(name: name) } }
    func invite(_ peerId: String) { run { try $0.invite(peerId: peerId) } }
    func connect(_ peerId: String) {
        if !inRoom { createRoom(name: defaultRoomName) }
        invite(peerId)
    }
    func respondToInvite(accept: Bool) {
        guard let inv = incomingInvite else { return }
        incomingInvite = nil
        inviteAnswered()
        run { try $0.respondToInvite(roomId: inv.id, accept: accept) }
    }
    func leaveRoom() { run { try $0.leaveRoom() } }
    func setCoordinator(_ id: String) { coordinatorLostCandidates = nil; run { try $0.setCoordinator(peerId: id) } }
    func setSpeaker(_ id: String?) { speakerLostCandidates = nil; run { try $0.setSpeaker(peerId: id) } }
    func setMicEnabled(_ id: String, _ on: Bool) { run { try $0.setPeerMicrophoneEnabled(peerId: id, enabled: on) } }
    func setUseMyMic(_ on: Bool) { setMicEnabled(localPeerId, on) }
    func toggleMute() { isMuted.toggle(); core?.setLocalMute(muted: isMuted) }
    func metrics() -> [FfiPeerMetrics] { core?.getPeerMetrics() ?? [] }
    /// The noise baseline Settings › Audio edits (`nil` = Automatic): the room's while in a room
    /// (it applies to every Mac in it), else this Mac's default for rooms it creates.
    var noiseBaselineDb: Double? {
        guard let room else { return settings.noiseBaselineDb }
        return room.noiseBaselineDb.map(Double.init)
    }
    /// Sets the baseline `noiseBaselineDb` reads: the room's (through the coordinator; the new
    /// value comes back in the room state) or this Mac's default.
    func setNoiseBaseline(_ db: Double?) {
        if inRoom {
            run { try $0.setRoomNoiseBaseline(baselineDb: db.map(Float.init)) }
        } else {
            settings.noiseBaselineDb = db
        }
    }
    /// The live mic meter behind Settings › Audio (see `NoiseMeterModel`).
    func setMicMeterEnabled(_ enabled: Bool) {
        meterRunning = enabled
        core?.setMicMeterEnabled(enabled: enabled)
        updateAudioActivity()
    }
    func micMeter() -> FfiMicMeter? { core?.getMicMeter() }
    /// Where audio was lost, for Settings › Advanced (`nil` before the core starts).
    func audioHealth() -> FfiAudioHealth? { core?.getAudioHealth() }
    func resetAudioHealth() { core?.resetAudioHealth() }
    /// Polled every 2 s and after driver changes; assigns only on change so views don't re-render needlessly.
    func refreshStatus() {
        let available = core?.virtualDeviceAvailable() ?? false
        if available != virtualDeviceAvailable { virtualDeviceAvailable = available }
        let mic = Permissions.microphone
        if mic != micPermission { micPermission = mic }
        let installed = VirtualDeviceStatus.installed
        let needs = DriverInstaller.needsInstall(bundled: DriverInstaller.bundledDriverURL,
                                                 installed: VirtualDeviceStatus.installedDriverURL, devicesPresent: installed)
        if needs != needsDriverInstall { needsDriverInstall = needs }
        let version = VirtualDeviceStatus.installedVersion
        if version != installedDriverVersion { installedDriverVersion = version }
        if installed != driverInstalled {
            driverInstalled = installed
            core?.setLocalInfo(name: Host.current().localizedName ?? "Mac", driverInstalled: installed)
        }
    }

    // MARK: driver install / uninstall
    func installDriver() { runDriverOperation(.install) }
    func uninstallDriver() { runDriverOperation(.uninstall) }

    private func runDriverOperation(_ op: DriverOperation) {
        guard driverOperation == nil else { return }
        if inRoom, !confirmAudioRestart() { return }
        driverOperation = op
        let work = driverWork
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            let result = Result { try work(op) }
            DispatchQueue.main.async { [weak self] in
                MainActor.assumeIsolated { self?.finishDriverOperation(op, result) }
            }
        }
    }

    private func finishDriverOperation(_ op: DriverOperation, _ result: Result<Bool, Error>) {
        switch result {
        case .failure(let error):
            driverOperation = nil
            lastError = describe(error)
            refreshStatus()
        case .success(false): // user dismissed the admin prompt
            driverOperation = nil
        case .success(true) where op == .install:
            awaitDevices(attempt: 0)
        case .success(true):
            driverOperation = nil
            refreshStatus()
        }
    }

    /// coreaudiod takes a moment to load the plug-in: poll for the devices for ~5 s.
    private func awaitDevices(attempt: Int) {
        let present = devicesPresent()
        if present || attempt >= 10 {
            driverOperation = nil
            refreshStatus()
            if present {
                showNotice("RoomMesh driver installed. Quit and reopen your browser and meeting apps (Meet, Zoom, Teams) so they see the RoomMesh devices.", clearAfter: 20)
            } else {
                showNotice("The RoomMesh devices haven't appeared yet. Restart the Mac to finish installing the driver.", clearAfter: 20)
            }
            return
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) { [weak self] in
            MainActor.assumeIsolated { self?.awaitDevices(attempt: attempt + 1) }
        }
    }

    /// Shows a transient, non-error status line and clears any stale error. A newer notice
    /// restarts the timer; only the latest one is cleared.
    func showNotice(_ message: String, clearAfter seconds: TimeInterval = 4) {
        lastError = nil
        notice = message
        noticeGeneration &+= 1
        let generation = noticeGeneration
        DispatchQueue.main.asyncAfter(deadline: .now() + seconds) { [weak self] in
            MainActor.assumeIsolated {
                guard let self, self.noticeGeneration == generation else { return }
                self.notice = nil
            }
        }
    }

    // MARK: events
    func apply(_ e: FfiEvent) {
        switch e {
        case .nearbyChanged(let peers): nearby = peers
        case .roomChanged(let state): room = state
        case .inviteReceived(let roomId, let roomName, let fromPeer, let fromName, let sas):
            // Only one invite is shown at a time: a newer one for another room declines the old one,
            // so its sender isn't left waiting. (Its notification is replaced by the new one's.)
            if let old = incomingInvite, old.id != roomId { run { try $0.respondToInvite(roomId: old.id, accept: false) } }
            incomingInvite = PendingInvite(id: roomId, roomName: roomName, fromPeer: fromPeer, fromName: fromName, sas: sas)
            getAttention(.invite, invite: (roomName, fromName))
        case .inviteDeclined(let peerId): lastError = "\(name(of: peerId) ?? "The other Mac") declined the invitation."
        case .coordinatorLost(let c): coordinatorLostCandidates = c; getAttention(.coordinatorLost)
        case .speakerLost(let c): speakerLostCandidates = c; getAttention(.speakerLost)
        case .coordinatorChanged: coordinatorLostCandidates = nil
        case .speakerChanged(let p): if p != nil { speakerLostCandidates = nil }
        case .connectionQualityChanged(let p, let q): qualities[p] = q
        case .aecStatusChanged(let c): aecConverged = c
        case .leftRoom:
            room = nil; coordinatorLostCandidates = nil; speakerLostCandidates = nil; qualities = [:]
            showInviteSheet = false; aecConverged = false
        case .error(let m): lastError = m
        case .notice(let m): showNotice(m)
        case .peerJoined, .peerLeft, .activeMicChanged: break // reflected by the following .roomChanged
        }
    }
}
