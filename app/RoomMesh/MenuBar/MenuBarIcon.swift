import SwiftUI

struct MenuBarIcon: View {
    @Environment(AppModel.self) private var model
    var body: some View { Image(systemName: Self.symbol(for: model.iconState)) }
    static func symbol(for s: MenuIconState) -> String {
        switch s {
        case .notConnected: "mic.circle"
        case .connected: "mic.circle.fill"
        case .muted: "mic.slash.circle.fill"
        case .warning: "exclamationmark.circle.fill"
        }
    }
}
