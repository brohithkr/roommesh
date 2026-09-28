import SwiftUI

struct PeerRow: View {
    let member: FfiMember
    @Environment(AppModel.self) private var model
    var body: some View {
        HStack(spacing: 8) {
            Circle().fill(member.online ? Color.green : Color.gray).frame(width: 7, height: 7)
            Text(member.name + (member.isLocal ? " (This Mac)" : "")).lineLimit(1)
            Spacer()
            Button { model.setMicEnabled(member.id, !member.micEnabled) } label: {
                Image(systemName: member.micEnabled ? "mic.fill" : "mic.slash")
            }
            .buttonStyle(.borderless)
            .help(member.micEnabled ? "Stop using this microphone" : "Use this microphone")
            if member.isActiveMic {
                Text("ACTIVE").font(.caption2.bold()).padding(.horizontal, 5).padding(.vertical, 1)
                    .background(Color.green.opacity(0.2), in: Capsule())
            }
            if member.isSpeaker { Image(systemName: "speaker.wave.2.fill").help("Room speaker") }
            if member.isCoordinator { Image(systemName: "star.circle").help("Coordinator") }
        }
        .font(.callout)
    }
}
