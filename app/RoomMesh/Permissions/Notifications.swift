import Foundation

/// Stub until Task 39 adds the UserNotifications implementation.
final class Notifications: NSObject, @unchecked Sendable {
    static let shared = Notifications()
    func inviteReceived(roomName: String, from: String) {}
}
