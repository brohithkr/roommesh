import SwiftUI

struct AdvancedSettings: View {
    /// The Settings window widens to this on the Advanced tab so all metric columns fit.
    static let width: CGFloat = 800
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
                    TableColumn("Mac") { Text($0.name.isEmpty ? $0.peerId : $0.name).lineLimit(1) }.width(min: 90, ideal: 110)
                    TableColumn("RTT") { Text(fmt($0.rttMs, " ms")) }.width(52)
                    TableColumn("Jitter") { Text(fmt($0.jitterMs, " ms")) }.width(52)
                    TableColumn("Loss") { Text(fmt($0.lossPct, "%")) }.width(44)
                    TableColumn("Offset") { Text(fmt($0.clockOffsetMs, " ms", 2)) }.width(64)
                    TableColumn("Drift") { Text(fmt($0.driftPpm, " ppm")) }.width(60)
                    TableColumn("Buffer") { Text(fmt($0.bufferMs, " ms")) }.width(54)
                    TableColumn("Score") { Text(fmt($0.micScore, "", 2)) }.width(40)
                    TableColumn("AEC") { Text(fmt($0.aecErleDb, " dB")) }.width(52)
                    TableColumn("Active") { m in
                        Text(m.isActive ? "●" : "").accessibilityLabel(m.isActive ? "Active" : "Not active")
                    }.width(44)
                }
            }
        }
        .padding(16)
    }
}

extension FfiPeerMetrics: Identifiable { public var id: String { peerId } }
