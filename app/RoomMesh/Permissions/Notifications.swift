import UserNotifications

final class Notifications: NSObject, UNUserNotificationCenterDelegate, @unchecked Sendable {
    static let shared = Notifications()
    static let inviteCategory = "ROOMMESH_INVITE"
    var onInviteAction: ((Bool) -> Void)?
    var onOpen: (() -> Void)?

    func setUp() {
        let c = UNUserNotificationCenter.current()
        c.delegate = self
        let join = UNNotificationAction(identifier: "JOIN", title: "Join", options: [.foreground])
        let decline = UNNotificationAction(identifier: "DECLINE", title: "Decline", options: [])
        c.setNotificationCategories([UNNotificationCategory(identifier: Self.inviteCategory, actions: [join, decline], intentIdentifiers: [])])
        c.requestAuthorization(options: [.alert, .sound]) { _, _ in }
    }
    func inviteReceived(roomName: String, from: String) {
        let content = UNMutableNotificationContent()
        content.title = "Join “\(roomName)”?"
        content.body = "\(from) invited this Mac to the room."
        content.categoryIdentifier = Self.inviteCategory
        UNUserNotificationCenter.current().add(UNNotificationRequest(identifier: "invite", content: content, trigger: nil))
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
