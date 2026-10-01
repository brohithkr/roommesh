import Foundation

/// One line of Settings › Advanced › Audio health.
struct AudioHealthRow: Equatable, Identifiable {
    let label: String
    let value: String
    var id: String { label }
}

/// Plain-language text for the audio health counters (see `FfiAudioHealth`).
enum AudioHealthText {
    static func realtime(_ status: FfiRealtimeStatus, demotions: UInt32) -> String {
        let base = switch status {
        case .workgroupAndTimeConstraint: "Real-time (audio workgroup)"
        case .timeConstraintOnly: "Real-time (no audio workgroup)"
        case .normalPriority: "Normal priority"
        }
        guard demotions > 0 else { return base }
        return base + ", lost real-time \(count(demotions, "time"))"
    }

    /// "0 ms", "1.8 ms", "120 ms".
    static func ms(_ v: Float) -> String {
        if v <= 0 { return "0 ms" }
        return v < 10 ? String(format: "%.1f ms", v) : String(format: "%.0f ms", v)
    }

    /// "1 frame", "3 frames".
    static func count<N: BinaryInteger>(_ n: N, _ unit: String) -> String {
        "\(n) \(unit)\(n == 1 ? "" : "s")"
    }

    /// "Last 60 seconds" (or less, right after audio started or the counters were reset).
    static func span(_ h: FfiAudioHealth) -> String {
        "Last \(count(max(h.windowSecs, 1), "second"))"
    }

    /// The lines for the last minute. `name`: a Mac's name for its peer id.
    static func rows(_ h: FfiAudioHealth, name: (String) -> String?) -> [AudioHealthRow] {
        let m = h.lastMinute
        var delay = ms(m.wakeWorstMs)
        if m.wakes > 0 && m.wakeWorstMs > 0 { delay += " (99% within \(ms(m.wakeP99Ms)))" }
        var underruns = "\(m.speakerUnderruns)"
        if m.speakerUnderruns > 0 { underruns += " (\(ms(m.speakerUnderrunMs)) of silence)" }
        var rows = [
            AudioHealthRow(label: "Audio thread", value: realtime(h.realtime, demotions: h.demotions)),
            AudioHealthRow(label: "Audio thread worst delay", value: delay),
            AudioHealthRow(label: "Speaker underruns", value: underruns),
            AudioHealthRow(label: "Gaps in meeting audio on the speaker", value: ms(m.speakerGapMs)),
            AudioHealthRow(label: "Late meeting audio dropped", value: count(m.lateMeetingFrames, "frame")),
            AudioHealthRow(label: "Meeting audio restarts", value: "\(m.meetingRestarts)"),
            AudioHealthRow(label: "Room mic silence after a stall", value: ms(m.micSilenceMs)),
        ]
        for missing in m.micMissing {
            let who = name(missing.peerId) ?? missing.peerId
            rows.append(AudioHealthRow(label: "Missing mic audio from \(who)", value: ms(missing.missingMs)))
        }
        return rows
    }
}
