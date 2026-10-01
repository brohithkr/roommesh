import XCTest
@testable import RoomMesh

/// Counts begin/end calls in place of `ProcessInfo`'s activity API.
@MainActor
final class ActivityProbe {
    var begun = 0
    var ended: [NSObjectProtocol] = []
    private(set) var tokens: [NSObjectProtocol] = []
    lazy var activity = AudioActivity(begin: { [unowned self] in
        begun += 1
        let t = NSObject()
        tokens.append(t)
        return t
    }, end: { [unowned self] in ended.append($0) })
}

@MainActor
final class AudioActivityTests: XCTestCase {
    var probe: ActivityProbe!
    var core: FakeCore!
    var model: AppModel!

    override func setUp() async throws {
        probe = ActivityProbe()
        core = FakeCore()
        let defaults = UserDefaults(suiteName: "test.\(UUID())")!
        model = AppModel(settings: SettingsStore(defaults: defaults), audioActivity: probe.activity)
        model.attach(core: core)
    }

    func testUpdateTakesTheActivityOnceAndEndsTheSameToken() {
        let a = probe.activity
        XCTAssertFalse(a.isHeld)
        a.update(carryingAudio: false)
        XCTAssertEqual(probe.begun, 0, "nothing to end")
        XCTAssertTrue(probe.ended.isEmpty)
        for _ in 0..<3 { a.update(carryingAudio: true) }
        XCTAssertEqual(probe.begun, 1)
        XCTAssertTrue(a.isHeld)
        a.update(carryingAudio: false)
        a.update(carryingAudio: false)
        XCTAssertEqual(probe.ended.count, 1)
        XCTAssertTrue(probe.ended[0] === probe.tokens[0])
        XCTAssertFalse(a.isHeld)
    }

    func testJoiningARoomTakesItOnceAndLeavingReleasesIt() {
        XCTAssertFalse(probe.activity.isHeld, "not in a room")
        model.apply(.roomChanged(state: sampleRoom()))
        XCTAssertTrue(probe.activity.isHeld)
        // Room updates keep arriving while in the room: still one activity.
        for _ in 0..<5 { model.apply(.roomChanged(state: sampleRoom(noiseBaselineDb: -40))) }
        XCTAssertEqual(probe.begun, 1)
        XCTAssertTrue(probe.ended.isEmpty)
        model.apply(.leftRoom)
        XCTAssertFalse(probe.activity.isHeld)
        XCTAssertEqual(probe.ended.count, 1)
        // A second room takes a new one.
        model.apply(.roomChanged(state: sampleRoom()))
        XCTAssertEqual(probe.begun, 2)
    }

    func testTheNoiseMeterHoldsItOutsideARoom() {
        model.setMicMeterEnabled(true)
        XCTAssertEqual(core.calls.last, "meter:true")
        XCTAssertTrue(probe.activity.isHeld)
        // Joining while the meter runs: the same activity.
        model.apply(.roomChanged(state: sampleRoom()))
        XCTAssertEqual(probe.begun, 1)
        model.setMicMeterEnabled(false)
        XCTAssertTrue(probe.activity.isHeld, "still in the room")
        model.apply(.leftRoom)
        XCTAssertFalse(probe.activity.isHeld)
        XCTAssertEqual(probe.begun, 1)
        XCTAssertEqual(probe.ended.count, 1)
    }

    func testShutdownReleasesIt() {
        model.apply(.roomChanged(state: sampleRoom()))
        model.setMicMeterEnabled(true)
        model.shutdown()
        XCTAssertFalse(probe.activity.isHeld)
        XCTAssertEqual(probe.ended.count, 1)
        XCTAssertTrue(core.calls.contains("leave"))
    }

    func testHealthIsReadAndResetThroughTheCore() {
        core.health = emptyHealth(.workgroupAndTimeConstraint, lastMinute: healthCounts(late: 3))
        XCTAssertEqual(model.audioHealth()?.lastMinute.lateMeetingFrames, 3)
        model.resetAudioHealth()
        XCTAssertEqual(core.calls.last, "resetHealth")
    }
}
