import AppKit
import SwiftUI

@MainActor
final class MainWindowController: NSObject, NSWindowDelegate {
    static let shared = MainWindowController()
    private var window: NSWindow?
    private var level = FloatingLevel()
    /// Requested by `setFloating`; applied as soon as the window exists.
    private var wantsFloating = false
    var isVisible: Bool { window?.isVisible ?? false }

    override init() { super.init() }
    /// Test seam: control an existing window.
    init(window: NSWindow) { self.window = window; super.init() }

    /// Shows the window in front, even while another app is active: activation is only a request
    /// on macOS 14+, so the window is also ordered front regardless.
    func show() {
        if window == nil {
            let host = NSHostingController(rootView: MainWindowView().environment(AppModel.shared))
            let w = NSWindow(contentViewController: host)
            w.title = "RoomMesh"
            w.styleMask = [.titled, .closable, .miniaturizable]
            w.isReleasedWhenClosed = false
            w.setContentSize(NSSize(width: 380, height: 520))
            w.center()
            w.delegate = self
            window = w
            applyLevel()
        }
        NSApp.activate()
        window?.makeKeyAndOrderFront(nil)
        window?.orderFrontRegardless()
    }

    /// Floats the window above other apps (while a prompt waits and banners are unavailable), or
    /// puts back the level it had before.
    func setFloating(_ on: Bool) {
        guard on != wantsFloating else { return }
        wantsFloating = on
        applyLevel()
    }

    private func applyLevel() {
        guard let window else { return }
        window.level = wantsFloating ? level.raise(from: window.level) : level.restore(current: window.level)
    }
}
