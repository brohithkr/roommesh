import SwiftUI

struct FailurePromptView: View {
    enum Kind { case coordinator, speaker }
    let kind: Kind
    let candidates: [String]
    @Environment(AppModel.self) private var model
    @State private var choice: String = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(kind == .coordinator ? "Coordinator disconnected." : "Room speaker disconnected.").font(.headline)
            Text(kind == .coordinator ? "Select new coordinator:" : "Select another speaker:").foregroundStyle(.secondary)
            if candidates.isEmpty {
                Text("No other Mac is available right now.").foregroundStyle(.secondary)
            } else {
                Picker("", selection: $choice) {
                    ForEach(candidates, id: \.self) { id in Text(model.name(of: id) ?? id).tag(id) }
                }
                .pickerStyle(.radioGroup)
                .labelsHidden()
            }
            HStack {
                Button("Later") { dismissPrompt() }
                Spacer()
                if kind == .speaker, let coord = model.room?.coordinator, candidates.contains(coord) {
                    Button("Use Coordinator Mac") { model.setSpeaker(coord) }
                }
                Button("Select") {
                    if kind == .coordinator { model.setCoordinator(choice) } else { model.setSpeaker(choice) }
                }
                .disabled(choice.isEmpty)
                .keyboardShortcut(.defaultAction)
            }
        }
        .padding(20)
        .frame(width: 340)
        .onAppear { choice = candidates.first ?? "" }
    }
    private func dismissPrompt() {
        if kind == .coordinator { model.coordinatorLostCandidates = nil } else { model.speakerLostCandidates = nil }
    }
}
