import XCTest
@testable import RoomMesh

@MainActor
final class AppModelTests: XCTestCase {
    var core: FakeCore!
    var model: AppModel!

    override func setUp() async throws {
        core = FakeCore()
        model = AppModel(settings: SettingsStore(defaults: UserDefaults(suiteName: "test.\(UUID())")!))
        model.presentWindow = {}
        model.notifyInvite = { _, _ in }
        model.attach(core: core)
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
        let s = SettingsStore(defaults: UserDefaults(suiteName: "test.\(UUID())")!)
        XCTAssertEqual(s.ffi.micLatencyMs, 70, "matches the core default")
        s.allowSimultaneousTalkers = true
        s.micLatencyMs = 90
        XCTAssertTrue(s.ffi.allowSimultaneousTalkers)
        XCTAssertEqual(s.ffi.micLatencyMs, 90)
        XCTAssertEqual(s.peerId(), s.peerId(), "peer id is persisted")
    }
    func testEventRelayPreservesOrderAcrossThreads() {
        model.apply(.roomChanged(state: sampleRoom()))
        let relay = EventRelay(model: model)
        let done = expectation(description: "relayed")
        DispatchQueue.global().async {
            relay.onEvent(event: .leftRoom)
            relay.onEvent(event: .roomChanged(state: sampleRoom()))
            DispatchQueue.main.async { done.fulfill() }
        }
        wait(for: [done], timeout: 2)
        XCTAssertEqual(model.room?.roomId, "00000000000000ff", "roomChanged must be applied after leftRoom")
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
}
