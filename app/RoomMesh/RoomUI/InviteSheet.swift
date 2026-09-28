import SwiftUI

struct InviteSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @State private var invited: Set<String> = []
    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(model.room?.name ?? "Room").font(.headline)
            Text("Invite nearby Macs").foregroundStyle(.secondary)
            if model.invitableNearby.isEmpty {
                Text("No other Macs running RoomMesh were found nearby.").foregroundStyle(.secondary)
            }
            ForEach(model.invitableNearby, id: \.id) { p in
                HStack {
                    Image(systemName: invited.contains(p.id) ? "checkmark.circle.fill" : "laptopcomputer")
                    VStack(alignment: .leading) {
                        Text(p.name)
                        if invited.contains(p.id), let sas = p.sas {
                            Text("Code \(sas) — check it matches on that Mac").font(.caption).foregroundStyle(.secondary)
                        }
                    }
                    Spacer()
                    Button(invited.contains(p.id) ? "Invited" : "Invite") { invited.insert(p.id); model.invite(p.id) }
                        .disabled(invited.contains(p.id))
                }
            }
            HStack { Spacer(); Button("Done") { dismiss() }.keyboardShortcut(.defaultAction) }
        }
        .padding(20)
        .frame(width: 360)
    }
}
