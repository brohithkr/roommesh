import SwiftUI

struct AdvancedSettings: View {
    @Environment(AppModel.self) private var model
    private func fmt(_ v: Float?, _ unit: String, _ digits: Int = 1) -> String {
        guard let v else { return "—" }
        return String(format: "%.\(digits)f", v) + unit
    }
    var body: some View {
        @Bindable var settings = model.settings
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                Stepper("Mic latency: \(settings.micLatencyMs) ms", value: $settings.micLatencyMs, in: 30...200, step: 10)
                Stepper("Speaker delay: \(settings.playoutDelayMs) ms", value: $settings.playoutDelayMs, in: 40...250, step: 10)
            }
            Text("AEC: \(model.aecConverged ? "converged" : "adapting")").font(.caption).foregroundStyle(.secondary)
            TimelineView(.periodic(from: .now, by: 1)) { _ in
                let rows = model.metrics()
                Table(rows) {
                    TableColumn("Mac") { Text($0.name.isEmpty ? $0.peerId : $0.name) }
                    TableColumn("RTT") { Text(fmt($0.rttMs, " ms")) }
                    TableColumn("Jitter") { Text(fmt($0.jitterMs, " ms")) }
                    TableColumn("Loss") { Text(fmt($0.lossPct, "%")) }
                    TableColumn("Offset") { Text(fmt($0.clockOffsetMs, " ms", 2)) }
                    TableColumn("Drift") { Text(fmt($0.driftPpm, " ppm")) }
                    TableColumn("Buffer") { Text(fmt($0.bufferMs, " ms")) }
                    TableColumn("Score") { Text(fmt($0.micScore, "", 2)) }
                    TableColumn("AEC") { Text(fmt($0.aecErleDb, " dB")) }
                    TableColumn("Active") { Text($0.isActive ? "●" : "") }
                }
            }
        }
        .padding(16)
    }
}

extension FfiPeerMetrics: Identifiable { public var id: String { peerId } }
