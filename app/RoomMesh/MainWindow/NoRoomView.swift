import SwiftUI

struct NoRoomView: View {
    @Environment(AppModel.self) private var model
    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("No active room").font(.headline).foregroundStyle(.secondary)
            GroupBox("Nearby Macs") {
                VStack(alignment: .leading, spacing: 6) {
                    if model.nearby.isEmpty {
                        HStack(spacing: 8) {
                            ProgressView().controlSize(.small)
                            Text("Looking for Macs running RoomMesh…").foregroundStyle(.secondary)
                        }
                    } else {
                        ForEach(model.nearby, id: \.id) { p in
                            HStack {
                                Image(systemName: "laptopcomputer")
                                Text(p.name).lineLimit(1)
                                Spacer()
                                Button("Connect") { model.connect(p.id) }
                            }
                        }
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.vertical, 4)
            }
            HStack {
                Spacer()
                Button("Create Room") { model.createRoom(name: model.defaultRoomName) }
                    .buttonStyle(.borderedProminent)
                    .controlSize(.large)
                Spacer()
            }
        }
    }
}
