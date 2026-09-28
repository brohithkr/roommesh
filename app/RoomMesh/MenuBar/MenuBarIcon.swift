import SwiftUI

struct MenuBarIcon: View {
    @Environment(AppModel.self) private var model
    var body: some View {
        Image(systemName: Self.symbol(for: model.iconState)).accessibilityLabel(Self.accessibilityLabel(for: model.iconState))
    }
    static func accessibilityLabel(for s: MenuIconState) -> String {
        switch s {
        case .notConnected: "RoomMesh, not in a room"
        case .connected: "RoomMesh, connected"
        case .muted: "RoomMesh, microphone muted"
        case .warning: "RoomMesh, needs attention"
        }
    }
    static func symbol(for s: MenuIconState) -> String {
        switch s {
        case .notConnected: "mic.circle"
        case .connected: "mic.circle.fill"
        case .muted: "mic.slash.circle.fill"
        case .warning: "exclamationmark.circle.fill"
        }
    }
}
