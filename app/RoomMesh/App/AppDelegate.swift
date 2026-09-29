import AppKit

final class AppDelegate: NSObject, NSApplicationDelegate {
    private var statusTimer: Timer?

    func applicationDidFinishLaunching(_ notification: Notification) {
        // Unit tests are hosted in the app: don't start the core, Bonjour or permission prompts there.
        guard !Self.isHostingUnitTests else { return }
        MainActor.assumeIsolated {
            let model = AppModel.shared
            // Handlers first: a launch from a notification action is delivered as soon as the delegate is set.
            Notifications.shared.onInviteAction = { accept in
                DispatchQueue.main.async { MainActor.assumeIsolated { AppModel.shared.respondToInvite(accept: accept) } }
            }
            Notifications.shared.onOpen = {
                DispatchQueue.main.async { MainActor.assumeIsolated { MainWindowController.shared.show() } }
            }
            Notifications.shared.onStatusChange = { status in
                DispatchQueue.main.async {
                    MainActor.assumeIsolated {
                        let model = AppModel.shared
                        if model.notificationsAllowed != status.allowed { model.notificationsAllowed = status.allowed }
                        if model.notificationStatus != status.label { model.notificationStatus = status.label }
                    }
                }
            }
            Notifications.shared.setUp()
            // The user may change it in System Settings at any time: re-read it whenever we're back.
            NotificationCenter.default.addObserver(forName: NSApplication.didBecomeActiveNotification, object: nil, queue: .main) { _ in
                Notifications.shared.refreshStatus()
            }
            model.bootstrap(driverInstalled: VirtualDeviceStatus.installed)
            model.refreshStatus()
            model.updates.startAutomaticChecks()
            if Permissions.microphone == .undetermined { Task { _ = await Permissions.requestMicrophone() } }
            if model.settings.showWindowAtLaunch && !Self.launchedAsLoginItem { MainWindowController.shared.show() }
            // .common mode: keep polling while a menu is open or a control is tracking.
            let timer = Timer(timeInterval: 2, repeats: true) { _ in
                MainActor.assumeIsolated { AppModel.shared.refreshStatus() }
            }
            RunLoop.main.add(timer, forMode: .common)
            statusTimer = timer
        }
    }
    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        MainActor.assumeIsolated { MainWindowController.shared.show() }
        return true
    }
    func applicationWillTerminate(_ notification: Notification) {
        MainActor.assumeIsolated { AppModel.shared.shutdown() }
    }
    static var launchedAsLoginItem: Bool {
        guard let e = NSAppleEventManager.shared().currentAppleEvent else { return false }
        return e.eventID == kAEOpenApplication
            && e.paramDescriptor(forKeyword: keyAEPropData)?.enumCodeValue == keyAELaunchedAsLogInItem
    }
    static var isHostingUnitTests: Bool { ProcessInfo.processInfo.environment["XCTestConfigurationFilePath"] != nil }
}
