import XCTest
@testable import RoomMesh

@MainActor
final class AppModelTests: XCTestCase {
    var core: FakeCore!
    var model: AppModel!
    private var suites: [String] = []

    /// A fresh preferences suite, removed again in tearDown.
    func makeDefaults(_ name: String = "test.\(UUID())") -> UserDefaults {
        if !suites.contains(name) { suites.append(name) }
        return UserDefaults(suiteName: name)!
    }

    override func setUp() async throws {
        core = FakeCore()
        model = AppModel(settings: SettingsStore(defaults: makeDefaults()))
        model.presentWindow = {}
        model.notifyInvite = { _, _ in }
        model.driverWork = { _ in XCTFail("driver work must be stubbed per test"); return false }
        model.confirmAudioRestart = { XCTFail("unexpected confirmation"); return false }
        model.attach(core: core)
    }

    override func tearDown() async throws {
        for name in suites { UserDefaults.standard.removePersistentDomain(forName: name) }
        suites = []
    }

    func testDerivedRoomInfo() {
        model.apply(.roomChanged(state: sampleRoom()))
        XCTAssertEqual(model.coordinatorName, "Rohith's MacBook")
        XCTAssertEqual(model.speakerName, "Meeting MacBook")
        XCTAssertEqual(model.activeMicName, "Amaan's MacBook")
        XCTAssertEqual(model.connectedCount, 3)
        XCTAssertTrue(model.isLocalCoordinator)
        XCTAssertEqual(model.iconState, .connected)
    }
    func testConnectCreatesRoomThenInvites() {
        model.connect("000000000000000b")
        XCTAssertEqual(core.calls.first?.hasPrefix("create:"), true)
        XCTAssertEqual(core.calls.last, "invite:000000000000000b")
    }
    func testInviteFlowAndSheetPriority() {
        model.apply(.inviteReceived(roomId: "00000000000000ff", roomName: "R", fromPeer: "000000000000000b", fromName: "Amaan", sas: "123 456"))
        model.apply(.speakerLost(candidates: ["000000000000000a"]))
        guard case .incomingInvite(let inv) = model.activeSheet else { return XCTFail("invite must win") }
        XCTAssertEqual(inv.sas, "123 456")
        model.respondToInvite(accept: true)
        XCTAssertEqual(core.calls.last, "respond:00000000000000ff:true")
        guard case .speakerLost = model.activeSheet else { return XCTFail("speaker prompt next") }
        model.setSpeaker("000000000000000a")
        XCTAssertNil(model.activeSheet)
    }
    func testMuteAndWarningIcon() {
        model.apply(.roomChanged(state: sampleRoom()))
        model.toggleMute()
        XCTAssertEqual(core.calls.last, "mute:true")
        XCTAssertEqual(model.iconState, .muted)
        model.apply(.connectionQualityChanged(peerId: "000000000000000b", quality: .degraded))
        XCTAssertEqual(model.iconState, .warning)
        model.apply(.leftRoom)
        XCTAssertEqual(model.iconState, .notConnected)
    }
    func testErrorsSurfaceAsMessages() {
        core.fail = NSError(domain: "x", code: 1, userInfo: [NSLocalizedDescriptionKey: "boom"])
        model.leaveRoom()
        XCTAssertEqual(model.lastError, "boom")
    }
    func testFfiErrorsSurfaceAsPlainMessages() {
        core.fail = FfiError.NotInRoom(message: "not in a room")
        model.leaveRoom()
        XCTAssertEqual(model.lastError, "not in a room")
    }
    func testSettingsMapToFfi() {
        let s = SettingsStore(defaults: makeDefaults())
        XCTAssertEqual(s.ffi.micLatencyMs, 70, "matches the core default")
        s.allowSimultaneousTalkers = true
        s.micLatencyMs = 90
        XCTAssertTrue(s.ffi.allowSimultaneousTalkers)
        XCTAssertEqual(s.ffi.micLatencyMs, 90)
        XCTAssertEqual(s.peerId(), s.peerId(), "peer id is persisted")
    }
    func testEventRelayPreservesPerThreadOrder() {
        // Several "Rust threads" each emit a numbered sequence; every thread's events must be applied
        // in emission order. Per-event `Task { @MainActor }` hops reorder these; the main queue does not.
        let threads = 4, perThread = 300
        var applied: [Int: [Int]] = [:]
        let done = expectation(description: "all events applied")
        var count = 0
        let relay = EventRelay { event in
            guard case .error(let m) = event else { return }
            let parts = m.split(separator: ":").compactMap { Int($0) }
            applied[parts[0], default: []].append(parts[1])
            count += 1
            if count == threads * perThread { done.fulfill() }
        }
        for t in 0..<threads {
            Thread.detachNewThread {
                for i in 0..<perThread { relay.onEvent(event: .error(message: "\(t):\(i)")) }
            }
        }
        wait(for: [done], timeout: 10)
        for t in 0..<threads { XCTAssertEqual(applied[t], Array(0..<perThread), "thread \(t) events reordered") }
    }
    func testEventRelayDeliversToModel() {
        let relay = EventRelay(model: model)
        let done = expectation(description: "relayed")
        DispatchQueue.global().async {
            relay.onEvent(event: .roomChanged(state: sampleRoom()))
            relay.onEvent(event: .leftRoom)
            DispatchQueue.main.async { done.fulfill() }
        }
        wait(for: [done], timeout: 2)
        XCTAssertNil(model.room, "leftRoom must be applied after roomChanged")
    }
    func testNoticeIsTransientAndClearsError() {
        model.lastError = "old"
        model.showNotice("Invite timed out", clearAfter: 0.05)
        XCTAssertEqual(model.notice, "Invite timed out")
        XCTAssertNil(model.lastError)
        let cleared = expectation(description: "cleared")
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.2) { cleared.fulfill() }
        wait(for: [cleared], timeout: 2)
        XCTAssertNil(model.notice)
    }
    func testIconSymbols() {
        XCTAssertEqual(MenuBarIcon.symbol(for: .notConnected), "mic.circle")
        XCTAssertEqual(MenuBarIcon.symbol(for: .connected), "mic.circle.fill")
        XCTAssertEqual(MenuBarIcon.symbol(for: .muted), "mic.slash.circle.fill")
        XCTAssertEqual(MenuBarIcon.symbol(for: .warning), "exclamationmark.circle.fill")
    }
    // MARK: sheet dismissal
    func testStaleDismissDoesNotDeclineANewInvite() {
        model.apply(.coordinatorLost(candidates: ["000000000000000b"]))
        model.sheetDidPresent(model.activeSheet!)
        // An invite arrives while SwiftUI is dismissing the coordinator prompt.
        model.apply(.inviteReceived(roomId: "00000000000000ff", roomName: "R", fromPeer: "000000000000000b", fromName: "Amaan", sas: "123 456"))
        model.activeSheet = nil
        XCTAssertFalse(core.calls.contains { $0.hasPrefix("respond:") }, "the invite must not be auto-declined")
        guard case .incomingInvite = model.activeSheet else { return XCTFail("invite must still be shown") }
    }
    func testStaleDismissAfterRespondKeepsQueuedPrompt() {
        model.apply(.inviteReceived(roomId: "00000000000000ff", roomName: "R", fromPeer: "000000000000000b", fromName: "Amaan", sas: "123 456"))
        model.apply(.speakerLost(candidates: ["000000000000000a"]))
        model.sheetDidPresent(model.activeSheet!)
        model.respondToInvite(accept: true)
        model.activeSheet = nil // the invite sheet's own dismissal
        guard case .speakerLost = model.activeSheet else { return XCTFail("queued speaker prompt must survive") }
    }
    func testDismissingThePresentedSheetClearsIt() {
        model.apply(.inviteReceived(roomId: "00000000000000ff", roomName: "R", fromPeer: "000000000000000b", fromName: "Amaan", sas: "1"))
        model.sheetDidPresent(model.activeSheet!)
        model.activeSheet = nil
        XCTAssertEqual(core.calls.last, "respond:00000000000000ff:false", "closing the invite declines it")
        model.apply(.coordinatorLost(candidates: []))
        model.sheetDidPresent(model.activeSheet!)
        model.activeSheet = nil
        XCTAssertNil(model.activeSheet)
    }
    func testExplicitDismissOnlyClearsTheNamedSheet() {
        model.showInviteSheet = true
        model.apply(.speakerLost(candidates: ["000000000000000a"]))
        model.dismiss(.invite)
        XCTAssertFalse(model.showInviteSheet)
        guard case .speakerLost = model.activeSheet else { return XCTFail("speaker prompt untouched") }
    }

    // MARK: priority, icon, health
    func testCoordinatorLostOutranksSpeakerLost() {
        model.apply(.speakerLost(candidates: ["000000000000000a"]))
        model.apply(.coordinatorLost(candidates: ["000000000000000b"]))
        guard case .coordinatorLost = model.activeSheet else { return XCTFail("coordinator prompt first") }
        model.setCoordinator("000000000000000b")
        guard case .speakerLost = model.activeSheet else { return XCTFail("speaker prompt next") }
    }
    func testWarningIconWhileCoordinatorLost() {
        model.apply(.roomChanged(state: sampleRoom()))
        model.apply(.coordinatorLost(candidates: ["000000000000000b"]))
        XCTAssertEqual(model.iconState, .warning)
    }
    func testHealthGood() {
        model.apply(.roomChanged(state: sampleRoom()))
        model.apply(.connectionQualityChanged(peerId: "000000000000000b", quality: .excellent))
        model.apply(.connectionQualityChanged(peerId: "000000000000000c", quality: .good))
        XCTAssertEqual(model.health, .good)
        XCTAssertEqual(model.iconState, .connected)
    }
    func testHealthIgnoresOfflineMembers() {
        model.apply(.roomChanged(state: sampleRoom()))
        model.apply(.connectionQualityChanged(peerId: "000000000000000d", quality: .disconnected)) // Puyan is offline
        XCTAssertEqual(model.health, .excellent)
    }
    func testLeftRoomResetsRoomScopedState() {
        model.apply(.roomChanged(state: sampleRoom()))
        model.showInviteSheet = true
        model.apply(.aecStatusChanged(converged: true))
        model.apply(.leftRoom)
        XCTAssertFalse(model.showInviteSheet)
        XCTAssertFalse(model.aecConverged)
        XCTAssertNil(model.activeSheet)
    }

    // MARK: settings
    func testSettingsPersistAcrossInstances() {
        let name = "test.\(UUID())"
        let a = SettingsStore(defaults: makeDefaults(name))
        a.inputDevice = "USB Mic"
        a.echoCancellation = false
        a.micLatencyMs = 120
        a.fallbackSpeakerToCoordinator = true
        let id = a.peerId()
        let b = SettingsStore(defaults: makeDefaults(name))
        XCTAssertEqual(b.inputDevice, "USB Mic")
        XCTAssertFalse(b.echoCancellation)
        XCTAssertEqual(b.micLatencyMs, 120)
        XCTAssertTrue(b.fallbackSpeakerToCoordinator)
        XCTAssertEqual(b.peerId(), id)
    }
    func testInvalidStoredPeerIdIsRegenerated() {
        let d = makeDefaults()
        for bad in ["ZZZZZZZZZZZZZZZZ", "00ABCDEF01234567", "short", "00abcdef012345678"] {
            d.set(bad, forKey: "peerId")
            let id = SettingsStore(defaults: d).peerId()
            XCTAssertNotEqual(id, bad)
            XCTAssertTrue(SettingsStore.isValidPeerId(id), "\(id) is not 16 lowercase hex")
            XCTAssertEqual(d.string(forKey: "peerId"), id, "regenerated id is persisted")
        }
        d.set("00abcdef01234567", forKey: "peerId")
        XCTAssertEqual(SettingsStore(defaults: d).peerId(), "00abcdef01234567")
    }
    func testChangingASettingPushesItToTheCore() {
        model.settings.noiseSuppression = false
        XCTAssertEqual(core.calls.last, "settings")
    }
    // MARK: transport permission callbacks
    func testLocalNetworkDeniedThenAllowed() {
        let transport = AppleP2PTransport() // never started: no network
        model.observe(transport: transport)
        /// Fires a transport callback off-main (as Network.framework does), then waits until its main hop ran.
        func fire(_ callback: (@Sendable () -> Void)?) {
            let e = expectation(description: "callback applied")
            DispatchQueue.global().async { callback?(); DispatchQueue.main.async { e.fulfill() } }
            wait(for: [e], timeout: 2)
        }
        fire(transport.onLocalNetworkDenied)
        XCTAssertTrue(model.localNetworkDenied)
        fire(transport.onLocalNetworkAllowed)
        XCTAssertFalse(model.localNetworkDenied, "access granted later clears the banner")
    }
    // MARK: driver install/uninstall
    /// Waits until the model has finished its background driver operation.
    func waitForDriverOperation() {
        let e = expectation(description: "driver operation finished")
        Task { @MainActor [model] in
            while model?.driverOperation != nil { try? await Task.sleep(for: .milliseconds(10)) }
            e.fulfill()
        }
        wait(for: [e], timeout: 5)
    }
    func testDriverInstallInRoomNeedsConfirmation() {
        model.apply(.roomChanged(state: sampleRoom()))
        var asked = 0
        model.confirmAudioRestart = { asked += 1; return false }
        model.driverWork = { _ in XCTFail("declined: must not run"); return true }
        model.installDriver()
        XCTAssertEqual(asked, 1)
        XCTAssertNil(model.driverOperation)
    }
    func testDriverInstallCancelledByUserIsSilent() {
        model.driverWork = { op in XCTAssertEqual(op, .install); return false } // osascript -128
        model.installDriver()
        XCTAssertEqual(model.driverOperation, .install, "busy while the admin prompt is up")
        model.installDriver() // ignored while busy
        waitForDriverOperation()
        XCTAssertNil(model.lastError)
        XCTAssertNil(model.notice)
    }
    func testDriverUninstallFailureSurfacesMessage() {
        model.driverWork = { _ in throw DriverInstaller.InstallError.failed("rm: denied") }
        model.uninstallDriver()
        waitForDriverOperation()
        XCTAssertEqual(model.lastError, "rm: denied")
    }
}
