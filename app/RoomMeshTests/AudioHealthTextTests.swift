import XCTest
@testable import RoomMesh

final class AudioHealthTextTests: XCTestCase {
    private func value(_ rows: [AudioHealthRow], _ label: String) -> String? {
        rows.first { $0.label == label }?.value
    }

    func testAQuietMinuteReadsAsZeros() {
        let rows = AudioHealthText.rows(emptyHealth(.workgroupAndTimeConstraint, lastMinute: healthCounts(wakes: 30_000)), name: { _ in nil })
        XCTAssertEqual(rows.map(\.label), [
            "Audio thread", "Audio thread worst delay", "Speaker underruns", "Gaps in meeting audio on the speaker",
            "Late meeting audio dropped", "Meeting audio restarts", "Room mic silence after a stall",
        ])
        XCTAssertEqual(value(rows, "Audio thread"), "Real-time (audio workgroup)")
        XCTAssertEqual(value(rows, "Audio thread worst delay"), "0 ms")
        XCTAssertEqual(value(rows, "Speaker underruns"), "0")
        XCTAssertEqual(value(rows, "Gaps in meeting audio on the speaker"), "0 ms")
        XCTAssertEqual(value(rows, "Late meeting audio dropped"), "0 frames")
        XCTAssertEqual(value(rows, "Meeting audio restarts"), "0")
        XCTAssertEqual(value(rows, "Room mic silence after a stall"), "0 ms")
    }

    func testLossesReadInPlainUnits() {
        let counts = healthCounts(wakes: 30_000, worst: 1.84, p99: 0.4, underruns: 2, underrunMs: 7.5, gapMs: 30,
                                  late: 3, restarts: 1, silenceMs: 40,
                                  missing: [FfiMicMissing(peerId: "000000000000000b", missingMs: 20),
                                            FfiMicMissing(peerId: "00000000000000ee", missingMs: 4.25)])
        let names = ["000000000000000b": "Amaan's MacBook"]
        let rows = AudioHealthText.rows(emptyHealth(.timeConstraintOnly, lastMinute: counts), name: { names[$0] })
        XCTAssertEqual(value(rows, "Audio thread"), "Real-time (no audio workgroup)")
        XCTAssertEqual(value(rows, "Audio thread worst delay"), "1.8 ms (99% within 0.4 ms)")
        XCTAssertEqual(value(rows, "Speaker underruns"), "2 (7.5 ms of silence)")
        XCTAssertEqual(value(rows, "Gaps in meeting audio on the speaker"), "30 ms")
        XCTAssertEqual(value(rows, "Late meeting audio dropped"), "3 frames")
        XCTAssertEqual(value(rows, "Meeting audio restarts"), "1")
        XCTAssertEqual(value(rows, "Room mic silence after a stall"), "40 ms")
        XCTAssertEqual(value(rows, "Missing mic audio from Amaan's MacBook"), "20 ms")
        XCTAssertEqual(value(rows, "Missing mic audio from 00000000000000ee"), "4.2 ms", "unknown Macs show their id")
        XCTAssertEqual(AudioHealthText.rows(emptyHealth(lastMinute: healthCounts(late: 1)), name: { _ in nil })
            .first { $0.label == "Late meeting audio dropped" }?.value, "1 frame")
    }

    func testRealtimeStatusAndDemotions() {
        XCTAssertEqual(AudioHealthText.realtime(.normalPriority, demotions: 0), "Normal priority")
        XCTAssertEqual(AudioHealthText.realtime(.normalPriority, demotions: 1), "Normal priority, lost real-time 1 time")
        XCTAssertEqual(AudioHealthText.realtime(.workgroupAndTimeConstraint, demotions: 3),
                       "Real-time (audio workgroup), lost real-time 3 times")
    }

    func testMillisecondsAndSpan() {
        XCTAssertEqual(AudioHealthText.ms(0), "0 ms")
        XCTAssertEqual(AudioHealthText.ms(-1), "0 ms")
        XCTAssertEqual(AudioHealthText.ms(0.04), "0.0 ms")
        XCTAssertEqual(AudioHealthText.ms(9.94), "9.9 ms")
        XCTAssertEqual(AudioHealthText.ms(10), "10 ms")
        XCTAssertEqual(AudioHealthText.ms(1234.4), "1234 ms")
        XCTAssertEqual(AudioHealthText.span(emptyHealth(windowSecs: 60)), "Last 60 seconds")
        XCTAssertEqual(AudioHealthText.span(emptyHealth(windowSecs: 1)), "Last 1 second")
        XCTAssertEqual(AudioHealthText.span(emptyHealth(windowSecs: 0)), "Last 1 second")
    }
}
