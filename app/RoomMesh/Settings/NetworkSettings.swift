import SwiftUI

struct NetworkSettings: View {
    @Environment(AppModel.self) private var model
    var body: some View {
        Form {
            LabeledContent("Transport", value: "Apple peer-to-peer (Bonjour over AWDL / Wi-Fi)")
            LabeledContent("In use", value: model.metrics().first(where: { !$0.transport.isEmpty })?.transport ?? "—")
            LabeledContent("Local Network access", value: model.localNetworkDenied ? "Denied" : "Allowed")
            if model.localNetworkDenied {
                Button("Open Privacy Settings…") { Permissions.openPrivacySettings("Privacy_LocalNetwork") }
            }
            LabeledContent("Peer ID") { Text(model.localPeerId).font(.system(.body, design: .monospaced)).textSelection(.enabled) }
        }
        .formStyle(.grouped)
    }
}
