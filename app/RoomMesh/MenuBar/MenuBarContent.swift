import SwiftUI

struct MenuBarContent: View {
    @Environment(AppModel.self) private var model
    @Environment(\.openSettings) private var openSettings

    var body: some View {
        if let room = model.room {
            Text(room.name)
            Text("\(model.connectedCount) \(model.connectedCount == 1 ? "Mac" : "Macs") connected · \(model.health.label)")
            Divider()
            Menu("Coordinator: \(model.coordinatorName ?? "—")") {
                ForEach(room.members, id: \.id) { m in
                    // A Mac without the RoomMesh driver can't coordinate.
                    Toggle(m.driverInstalled ? m.name : "\(m.name) (no driver)",
                           isOn: Binding(get: { m.isCoordinator }, set: { if $0 { model.setCoordinator(m.id) } }))
                        .disabled(!m.online || !m.driverInstalled)
                }
            }
            Menu("Room Speaker: \(model.speakerName ?? "None")") {
                ForEach(room.members, id: \.id) { m in
                    Toggle(m.name, isOn: Binding(get: { m.isSpeaker }, set: { if $0 { model.setSpeaker(m.id) } }))
                        .disabled(!m.online)
                }
            }
            Text("Active Mic: \(model.activeMicName ?? "—")" + (model.secondaryMicName.map { " + \($0)" } ?? ""))
            Divider()
            Toggle("Use This Mac's Microphone", isOn: Binding(get: { model.useMyMic }, set: { model.setUseMyMic($0) }))
            Button(model.isMuted ? "Unmute My Microphone" : "Mute My Microphone") { model.toggleMute() }
                .keyboardShortcut("m", modifiers: [.command, .shift])
            Divider()
            Button("Invite Nearby Device…") { model.showInviteSheet = true; MainWindowController.shared.show() }
            Button("Manage Room…") { MainWindowController.shared.show() }
            Button("Audio Diagnostics…") { model.settingsTab = .advanced; NSApp.activate(); openSettings() }
            Button("Settings…") { model.settingsTab = .general; NSApp.activate(); openSettings() }
                .keyboardShortcut(",")
            Divider()
            Button("Leave Room") { model.leaveRoom() }
        } else {
            Text("No active room")
            if !model.nearby.isEmpty {
                Divider()
                Text("Nearby Macs")
                ForEach(model.nearby, id: \.id) { p in Button("Connect to \(p.name)") { model.connect(p.id) } }
            }
            Divider()
            Button("Create Room") { model.createRoom(name: model.defaultRoomName) }
            Button("Open RoomMesh…") { MainWindowController.shared.show() }
            Button("Settings…") { NSApp.activate(); openSettings() }.keyboardShortcut(",")
        }
        Divider()
        Button("Quit RoomMesh") { NSApp.terminate(nil) }.keyboardShortcut("q")
    }
}
