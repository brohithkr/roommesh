import AppKit
import UserNotifications
import XCTest
@testable import RoomMesh

/// The in-app fallback used when macOS won't show RoomMesh's notifications (e.g. an ad-hoc signed
/// build Notification Center never registered).
@MainActor
final class NotificationFallbackTests: XCTestCase {
    var core: FakeCore!
    var model: AppModel!
    var presented = 0
    var banners: [String] = []
    var sounds = 0
    var floating: [Bool] = []
    private let suite = "test.\(UUID())"

    override func setUp() async throws {
        core = FakeCore()
        model = AppModel(settings: SettingsStore(defaults: UserDefaults(suiteName: suite)!))
        presented = 0; banners = []; sounds = 0; floating = []
        model.presentWindow = { [unowned self] in presented += 1 }
        model.notifyInvite = { [unowned self] room, _ in banners.append(room) }
        model.inviteAnswered = {}
        model.inviteUIVisible = { false }
        model.playAttentionSound = { [unowned self] in sounds += 1 }
        model.setWindowFloating = { [unowned self] in floating.append($0) }
        model.attach(core: core)
    }
    override func tearDown() async throws { UserDefaults.standard.removePersistentDomain(forName: suite) }

    private func invite(_ room: String = "00000000000000f1") {
        model.apply(.inviteReceived(roomId: room, roomName: "R", fromPeer: "000000000000000b", fromName: "Amaan", sas: "1"))
    }

    // MARK: decision logic
    func testPlanWhenNotificationsAllowed() {
        let p = AttentionPlan.make(for: .invite, notificationsAllowed: true, uiVisible: false)
        XCTAssertEqual(p, AttentionPlan(postNotification: true, playSound: false, bringToFront: true))
        let visible = AttentionPlan.make(for: .invite, notificationsAllowed: true, uiVisible: true)
        XCTAssertEqual(visible, AttentionPlan(postNotification: false, playSound: false, bringToFront: true))
    }
    func testPlanWhenNotificationsOff() {
        let p = AttentionPlan.make(for: .invite, notificationsAllowed: false, uiVisible: false)
        XCTAssertEqual(p, AttentionPlan(postNotification: false, playSound: true, bringToFront: true))
        let visible = AttentionPlan.make(for: .invite, notificationsAllowed: false, uiVisible: true)
        XCTAssertEqual(visible, AttentionPlan(postNotification: false, playSound: false, bringToFront: true),
                       "the sheet is already on screen: no sound")
    }
    func testPlanWhileAuthorizationUnknownDoesBoth() {
        // Before the settings are read: post (harmless if it's dropped) and also use the fallback.
        let p = AttentionPlan.make(for: .invite, notificationsAllowed: nil, uiVisible: false)
        XCTAssertEqual(p, AttentionPlan(postNotification: true, playSound: true, bringToFront: true))
    }
    func testPromptsNeverPostANotification() {
        for kind in [AttentionKind.coordinatorLost, .speakerLost] {
            XCTAssertFalse(AttentionPlan.make(for: kind, notificationsAllowed: true, uiVisible: false).postNotification)
            XCTAssertTrue(AttentionPlan.make(for: kind, notificationsAllowed: false, uiVisible: false).playSound)
        }
    }
    func testNotificationStatusMapping() {
        XCTAssertEqual(NotificationStatus(authorization: .authorized, alerts: .enabled, requestError: nil).allowed, true)
        XCTAssertEqual(NotificationStatus(authorization: .authorized, alerts: .disabled, requestError: nil).allowed, false,
                       "allowed but banners turned off: nothing would appear")
        XCTAssertEqual(NotificationStatus(authorization: .denied, alerts: .disabled, requestError: nil).allowed, false)
        XCTAssertEqual(NotificationStatus(authorization: .provisional, alerts: .enabled, requestError: nil).allowed, false,
                       "provisional notifications arrive silently")
        let failed = NotificationStatus(authorization: .notDetermined, alerts: .notSupported, requestError: "not allowed")
        XCTAssertFalse(failed.allowed, "not determined after the request")
        XCTAssertTrue(failed.label.contains("not allowed"), failed.label)
    }

    // MARK: model wiring
    func testInviteWithNotificationsOffUsesTheFallback() {
        model.notificationsAllowed = false
        invite()
        XCTAssertEqual(banners, [])
        XCTAssertEqual(sounds, 1)
        XCTAssertEqual(presented, 1)
        XCTAssertTrue(model.showsAttentionBadge)
        XCTAssertEqual(floating, [true])
        model.respondToInvite(accept: true)
        XCTAssertFalse(model.showsAttentionBadge, "badge clears once answered")
        XCTAssertEqual(floating, [true, false], "window level restored after the sheet")
    }
    func testInviteWithNotificationsAllowedKeepsTheBanner() {
        model.notificationsAllowed = true
        invite()
        XCTAssertEqual(banners, ["R"])
        XCTAssertEqual(sounds, 0)
        XCTAssertEqual(presented, 1, "window still brought to the front")
        XCTAssertFalse(model.showsAttentionBadge)
        XCTAssertEqual(floating, [], "no floating window when banners work")
    }
    func testDecliningByClosingTheSheetRestoresTheLevel() {
        model.notificationsAllowed = false
        invite()
        model.sheetDidPresent(model.activeSheet!)
        model.activeSheet = nil
        XCTAssertEqual(floating, [true, false])
    }
    func testCoordinatorAndSpeakerPromptsUseTheFallback() {
        model.notificationsAllowed = false
        model.apply(.coordinatorLost(candidates: ["000000000000000b"]))
        XCTAssertEqual(sounds, 1)
        XCTAssertTrue(model.showsAttentionBadge)
        model.apply(.speakerLost(candidates: ["000000000000000b"]))
        XCTAssertEqual(sounds, 2)
        XCTAssertEqual(floating, [true], "already floating")
        model.setCoordinator("000000000000000b")
        XCTAssertEqual(floating, [true], "speaker prompt still up")
        model.setSpeaker("000000000000000b")
        XCTAssertEqual(floating, [true, false])
        XCTAssertFalse(model.showsAttentionBadge)
    }
    func testPromptsWithNotificationsAllowedDoNotBadge() {
        model.notificationsAllowed = true
        model.apply(.speakerLost(candidates: []))
        XCTAssertEqual(sounds, 0)
        XCTAssertFalse(model.showsAttentionBadge)
        XCTAssertEqual(presented, 1)
    }
    func testGrantingNotificationsLaterRestoresTheLevel() {
        model.notificationsAllowed = false
        invite()
        model.notificationsAllowed = true
        XCTAssertEqual(floating, [true, false])
        XCTAssertFalse(model.showsAttentionBadge)
    }
    func testNoSoundWhenTheSheetIsAlreadyOnScreen() {
        model.notificationsAllowed = false
        model.inviteUIVisible = { true }
        invite()
        XCTAssertEqual(sounds, 0)
        XCTAssertEqual(presented, 1)
    }
    func testNotificationsOffBannerOnlyOnceKnown() {
        XCTAssertFalse(model.showsNotificationsOffBanner, "unknown yet: don't flash the banner at launch")
        model.notificationsAllowed = false
        XCTAssertTrue(model.showsNotificationsOffBanner)
        model.notificationsAllowed = true
        XCTAssertFalse(model.showsNotificationsOffBanner)
    }

    // MARK: window level restore
    func testFloatingLevelRestoresTheOriginalLevel() {
        var keeper = FloatingLevel()
        XCTAssertEqual(keeper.raise(from: .normal), .floating)
        XCTAssertEqual(keeper.raise(from: .floating), .floating, "raising twice keeps the saved level")
        XCTAssertEqual(keeper.restore(current: .floating), .normal)
        XCTAssertEqual(keeper.restore(current: .normal), .normal, "restore without a raise leaves the level alone")
    }
    func testFloatingLevelKeepsAHigherLevel() {
        var keeper = FloatingLevel()
        XCTAssertEqual(keeper.raise(from: .modalPanel), .modalPanel, "never lowers a window")
        XCTAssertEqual(keeper.restore(current: .modalPanel), .modalPanel)
    }
    func testWindowControllerAppliesAndRestoresTheLevel() {
        let window = NSWindow(contentRect: .init(x: 0, y: 0, width: 10, height: 10), styleMask: [.titled], backing: .buffered, defer: true)
        let controller = MainWindowController(window: window)
        controller.setFloating(true)
        XCTAssertEqual(window.level, .floating)
        controller.setFloating(false)
        XCTAssertEqual(window.level, .normal)
    }
}
