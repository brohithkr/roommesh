import AppKit
import UserNotifications

/// Something that needs the user's answer: an incoming invite or a failure prompt.
enum AttentionKind { case invite, coordinatorLost, speakerLost }

/// How to get the user's attention for an `AttentionKind`. When macOS won't show RoomMesh's
/// notifications (denied, never registered — common for ad-hoc signed builds — or unknown yet), the
/// app falls back to a sound, a floating front window and a menu-bar badge.
struct AttentionPlan: Equatable {
    var postNotification: Bool
    var playSound: Bool
    var bringToFront: Bool

    /// - notificationsAllowed: `nil` while the settings haven't been read yet; then both the
    ///   notification (harmless if dropped) and the fallback are used.
    /// - uiVisible: RoomMesh is active with its window up, so the sheet will be seen anyway.
    static func make(for kind: AttentionKind, notificationsAllowed: Bool?, uiVisible: Bool) -> AttentionPlan {
        let fallback = notificationsAllowed != true
        return AttentionPlan(postNotification: kind == .invite && notificationsAllowed != false && !uiVisible,
                             playSound: fallback && !uiVisible,
                             bringToFront: true)
    }
}

/// Remembers a window's own level while it floats above other apps, so it can be put back.
struct FloatingLevel {
    private(set) var saved: NSWindow.Level?

    /// The level to use while floating. Never lowers a window that is already higher.
    mutating func raise(from current: NSWindow.Level) -> NSWindow.Level {
        if saved == nil { saved = current }
        return current.rawValue >= NSWindow.Level.floating.rawValue ? current : .floating
    }
    /// The level to go back to; `current` if the window wasn't raised.
    mutating func restore(current: NSWindow.Level) -> NSWindow.Level {
        defer { saved = nil }
        return saved ?? current
    }
}

/// What Notification Center reports for RoomMesh, reduced to "will a banner appear?".
struct NotificationStatus: Equatable {
    let allowed: Bool
    /// Shown in Settings › General.
    let label: String

    init(authorization: UNAuthorizationStatus, alerts: UNNotificationSetting, requestError: String?) {
        switch authorization {
        case .authorized where alerts == .disabled:
            (allowed, label) = (false, "Allowed, but banners and alerts are turned off")
        case .authorized:
            (allowed, label) = (true, "On")
        case .denied:
            (allowed, label) = (false, "Off")
        case .provisional:
            (allowed, label) = (false, "Delivered quietly (no banners)")
        case .notDetermined:
            // After the request: macOS never registered the app (or the request failed).
            (allowed, label) = (false, requestError.map { "Not available (\($0))" } ?? "Not set up")
        @unknown default:
            (allowed, label) = (false, "Unknown")
        }
    }
}
