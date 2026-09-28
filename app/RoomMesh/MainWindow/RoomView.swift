import SwiftUI

struct MemberPicker: View {
    let title: String
    let selection: String?
    let members: [FfiMember]
    let onSelect: (String) -> Void
    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(title).font(.caption).foregroundStyle(.secondary)
            Picker(title, selection: Binding(get: { selection ?? "" }, set: { if !$0.isEmpty { onSelect($0) } })) {
                if selection == nil { Text("None").tag("") }
                ForEach(members, id: \.id) { m in Text(m.name).tag(m.id) }
            }
            .labelsHidden()
            .pickerStyle(.menu)
        }
    }
}

struct RoomView: View {
    @Environment(AppModel.self) private var model
    var body: some View {
        if let room = model.room {
            VStack(alignment: .leading, spacing: 14) {
                VStack(alignment: .leading, spacing: 2) {
                    Text(room.name).font(.headline)
                    Label(model.health == .degraded ? "Degraded" : "Connected", systemImage: "circle.fill")
                        .foregroundStyle(model.health == .degraded ? .orange : .green)
                        .font(.subheadline)
                    Text("\(model.connectedCount) \(model.connectedCount == 1 ? "Mac" : "Macs")").foregroundStyle(.secondary)
                }
                MemberPicker(title: "Coordinator", selection: room.coordinator, members: room.members.filter(\.online)) { model.setCoordinator($0) }
                MemberPicker(title: "Room Speaker", selection: room.speaker, members: room.members.filter(\.online)) { model.setSpeaker($0) }
                VStack(alignment: .leading, spacing: 2) {
                    Text("Active Microphone").font(.caption).foregroundStyle(.secondary)
                    Label(model.activeMicName ?? "—", systemImage: "waveform").foregroundStyle(model.activeMicName == nil ? .secondary : .primary)
                }
                Divider()
                ForEach(room.members, id: \.id) { PeerRow(member: $0) }
                HStack {
                    Button("Invite…") { model.showInviteSheet = true }
                    Spacer()
                    Button("Leave Room", role: .destructive) { model.leaveRoom() }
                }
                .padding(.top, 6)
            }
        }
    }
}
