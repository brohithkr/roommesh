import SwiftUI

/// Stub until the invite-flow task.
struct FailurePromptView: View {
    enum Kind { case coordinator, speaker }
    let kind: Kind
    let candidates: [String]
    var body: some View { Text(kind == .coordinator ? "Coordinator disconnected." : "Room speaker disconnected.") }
}
