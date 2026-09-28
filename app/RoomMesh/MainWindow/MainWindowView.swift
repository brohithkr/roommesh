import SwiftUI

struct MainWindowView: View {
    @Environment(AppModel.self) private var model
    var body: some View {
        @Bindable var model = model
        VStack(alignment: .leading, spacing: 0) {
            Text("RoomMesh").font(.title2.bold()).padding(.bottom, 12)
            if model.inRoom { RoomView() } else { NoRoomView() }
            Spacer(minLength: 8)
            StatusBanner()
        }
        .padding(20)
        .frame(width: 380)
        .frame(minHeight: 480)
        .sheet(item: $model.activeSheet) { sheet in
            switch sheet {
            case .incomingInvite(let invite): IncomingInviteView(invite: invite)
            case .coordinatorLost(let c): FailurePromptView(kind: .coordinator, candidates: c)
            case .speakerLost(let c): FailurePromptView(kind: .speaker, candidates: c)
            case .invite: InviteSheet()
            }
        }
    }
}
