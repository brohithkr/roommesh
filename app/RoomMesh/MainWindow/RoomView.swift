import SwiftUI

struct MemberPicker: View {
    let title: String
    let selection: String?
    let members: [FfiMember]
    /// Members without the RoomMesh driver are shown as "(no driver)" and can't be picked.
    var requiresDriver = false
    let onSelect: (String) -> Void
    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(title).font(.caption).foregroundStyle(.secondary)
            Picker(title, selection: Binding(get: { selection ?? "" }, set: { if !$0.isEmpty { onSelect($0) } })) {
                if selection == nil { Text("None").tag("") }
                ForEach(members, id: \.id) { m in
                    let unavailable = requiresDriver && !m.driverInstalled
                    Text(unavailable ? "\(m.name) (no driver)" : m.name).tag(m.id).disabled(unavailable)
                }
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
                MemberPicker(title: "Coordinator", selection: room.coordinator, members: room.members.filter(\.online),
                             requiresDriver: true) { model.setCoordinator($0) }
                MemberPicker(title: "Room Speaker", selection: room.speaker, members: room.members.filter(\.online)) { model.setSpeaker($0) }
                VStack(alignment: .leading, spacing: 2) {
                    Text(model.secondaryMicName == nil ? "Active Microphone" : "Active Microphones").font(.caption).foregroundStyle(.secondary)
                    Label(model.activeMicName ?? "—", systemImage: "waveform").foregroundStyle(model.activeMicName == nil ? .secondary : .primary)
                    if let secondary = model.secondaryMicName {
                        Label(secondary, systemImage: "waveform")
                    }
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
