import AppKit
import SwiftUI

struct MenuBarIcon: View {
    @Environment(AppModel.self) private var model
    var body: some View {
        let symbol = Self.symbol(for: model.iconState)
        let label = Self.accessibilityLabel(for: model.iconState)
        if model.showsAttentionBadge, let badged = Self.badged(symbol) {
            // MenuBarExtra labels render only a plain Image/Text, so the dot is drawn into the image.
            Image(nsImage: badged).accessibilityLabel("\(label), waiting for your answer")
        } else {
            Image(systemName: symbol).accessibilityLabel(label)
        }
    }
    /// The symbol with a dot in its top-right corner, as a template image (menu-bar colours).
    static func badged(_ symbol: String) -> NSImage? {
        guard let base = NSImage(systemSymbolName: symbol, accessibilityDescription: nil)?
            .withSymbolConfiguration(.init(pointSize: 15, weight: .regular)) else { return nil }
        let size = base.size
        let image = NSImage(size: size, flipped: false) { rect in
            base.draw(in: rect)
            let d = (size.width * 0.36).rounded()
            let dot = NSRect(x: rect.maxX - d, y: rect.maxY - d, width: d, height: d)
            NSGraphicsContext.current?.compositingOperation = .clear
            NSBezierPath(ovalIn: dot.insetBy(dx: -1.5, dy: -1.5)).fill()
            NSGraphicsContext.current?.compositingOperation = .sourceOver
            NSColor.black.setFill()
            NSBezierPath(ovalIn: dot).fill()
            return true
        }
        image.isTemplate = true
        return image
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
