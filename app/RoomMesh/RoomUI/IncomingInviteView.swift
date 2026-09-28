import SwiftUI

struct IncomingInviteView: View {
    let invite: PendingInvite
    @Environment(AppModel.self) private var model
    var body: some View {
        VStack(spacing: 14) {
            Image(systemName: "person.3.fill").font(.largeTitle).foregroundStyle(.tint)
            Text("Join “\(invite.roomName)”?").font(.title3.bold())
            Text("\(invite.fromName) invited this Mac to the room.").foregroundStyle(.secondary)
            if !invite.sas.isEmpty {
                VStack(spacing: 2) {
                    Text("Verification code").font(.caption).foregroundStyle(.secondary)
                    Text(invite.sas).font(.system(.title2, design: .monospaced).bold())
                    Text("Make sure \(invite.fromName) shows the same code.").font(.caption).foregroundStyle(.secondary)
                }
            }
            HStack {
                Button("Decline") { model.respondToInvite(accept: false) }.keyboardShortcut(.cancelAction)
                Button("Join") { model.respondToInvite(accept: true) }.keyboardShortcut(.defaultAction).buttonStyle(.borderedProminent)
            }
        }
        .padding(24)
        .frame(width: 340)
    }
}
