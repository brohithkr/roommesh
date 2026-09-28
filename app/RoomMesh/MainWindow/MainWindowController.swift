import AppKit

/// Stub until Task 41 builds the real window.
@MainActor
final class MainWindowController: NSObject, NSWindowDelegate {
    static let shared = MainWindowController()
    func show() {}
}
