import AppKit
import os
import UserNotifications

final class Notifications: NSObject, UNUserNotificationCenterDelegate, @unchecked Sendable {
    static let shared = Notifications()
    static let inviteCategory = "ROOMMESH_INVITE"
    /// Set once at launch, before `setUp()`; called from the notification center's queue.
    var onInviteAction: (@Sendable (Bool) -> Void)?
    var onOpen: (@Sendable () -> Void)?
    static let inviteIdentifier = "invite"
    /// Whether banners will appear, re-read at launch and whenever the app becomes active. Set once
    /// at launch, before `setUp()`; called from the notification center's queue.
    var onStatusChange: (@Sendable (NotificationStatus) -> Void)?
    private static let log = Logger(subsystem: Bundle.main.bundleIdentifier ?? "RoomMesh", category: "notifications")
    private let lock = NSLock()
    private var _requestError: String?
    private var _requestFinished = false
    /// The error `requestAuthorization` returned, if any (e.g. the app isn't registered).
    var requestError: String? { lock.withLock { _requestError } }

    func setUp() {
        let c = UNUserNotificationCenter.current()
        c.delegate = self
        let join = UNNotificationAction(identifier: "JOIN", title: "Join", options: [.foreground])
        let decline = UNNotificationAction(identifier: "DECLINE", title: "Decline", options: [])
        c.setNotificationCategories([UNNotificationCategory(identifier: Self.inviteCategory, actions: [join, decline], intentIdentifiers: [])])
        c.requestAuthorization(options: [.alert, .sound]) { [weak self] granted, error in
            guard let self else { return }
            let message = error.map { ($0 as NSError).localizedDescription }
            self.lock.withLock { self._requestError = message; self._requestFinished = true }
            if let error {
                let e = error as NSError
                Self.log.error("requestAuthorization failed: \(e.domain, privacy: .public) \(e.code) \(e.localizedDescription, privacy: .public)")
                NSLog("RoomMesh: notification authorization failed: %@ (%@ %ld)", e.localizedDescription, e.domain, e.code)
            } else {
                Self.log.notice("requestAuthorization granted=\(granted)")
                NSLog("RoomMesh: notification authorization granted=%@", granted ? "yes" : "no")
            }
            self.refreshStatus()
        }
    }
    /// Reads Notification Center's current settings for the app and reports them.
    func refreshStatus() {
        UNUserNotificationCenter.current().getNotificationSettings { [weak self] settings in
            guard let self else { return }
            // While the permission prompt is still up, "not determined" isn't an answer yet.
            let finished = self.lock.withLock { self._requestFinished }
            if settings.authorizationStatus == .notDetermined && !finished { return }
            let status = NotificationStatus(authorization: settings.authorizationStatus, alerts: settings.alertSetting,
                                            requestError: self.requestError)
            Self.log.notice("notification settings: authorization=\(settings.authorizationStatus.rawValue) alerts=\(settings.alertSetting.rawValue) allowed=\(status.allowed)")
            NSLog("RoomMesh: notification settings authorization=%ld alerts=%ld allowed=%@",
                  settings.authorizationStatus.rawValue, settings.alertSetting.rawValue, status.allowed ? "yes" : "no")
            self.onStatusChange?(status)
        }
    }
    /// System Settings › Notifications (the Ventura+ pane, else the older one).
    @MainActor static func openSettings() {
        for s in ["x-apple.systempreferences:com.apple.Notifications-Settings.extension",
                  "x-apple.systempreferences:com.apple.preference.notifications"] {
            if let url = URL(string: s), NSWorkspace.shared.open(url) { return }
        }
    }
    func inviteReceived(roomName: String, from: String) {
        let content = UNMutableNotificationContent()
        content.title = "Join “\(roomName)”?"
        content.body = "\(from) invited this Mac to the room."
        content.categoryIdentifier = Self.inviteCategory
        UNUserNotificationCenter.current().add(UNNotificationRequest(identifier: Self.inviteIdentifier, content: content, trigger: nil)) { error in
            if let error { NSLog("RoomMesh: invite notification not delivered: %@", error.localizedDescription) }
        }
    }
    /// The invite was answered (in the app or from the banner): drop its notification.
    func inviteAnswered() {
        let c = UNUserNotificationCenter.current()
        c.removeDeliveredNotifications(withIdentifiers: [Self.inviteIdentifier])
        c.removePendingNotificationRequests(withIdentifiers: [Self.inviteIdentifier])
    }
    func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse,
                                withCompletionHandler completionHandler: @escaping () -> Void) {
        switch response.actionIdentifier {
        case "JOIN": onInviteAction?(true)
        case "DECLINE": onInviteAction?(false)
        default: onOpen?()
        }
        completionHandler()
    }
    func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification,
                                withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void) {
        completionHandler([.banner, .sound])
    }
}
