import SwiftUI

@main
struct RoomMeshApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    var body: some Scene {
        MenuBarExtra("RoomMesh", systemImage: "mic.circle") {
            Text("RoomMesh core \(coreVersion())")
            Divider()
            Button("Quit RoomMesh") { NSApp.terminate(nil) }.keyboardShortcut("q")
        }
        .menuBarExtraStyle(.menu)
    }
}
