import XCTest
@testable import RoomMesh

@MainActor
final class NoiseMeterTests: XCTestCase {
    private var suites: [String] = []
    private func makeDefaults() -> UserDefaults {
        let name = "test.\(UUID())"
        suites.append(name)
        return UserDefaults(suiteName: name)!
    }
    override func tearDown() async throws {
        for name in suites { UserDefaults.standard.removePersistentDomain(forName: name) }
        suites = []
    }

    private func reading(level: Float, floor: Float, speech: Bool = false, peak: Float? = nil, auto: Float? = nil) -> FfiMicMeter {
        FfiMicMeter(levelDb: level, floorDb: floor, perceivedDb: max(0, level - floor), isSpeech: speech,
                    peakDb: peak ?? level, autoFloorDb: auto ?? floor)
    }

    // MARK: settings

    func testBaselineDefaultsToAutomaticAndPersists() {
        let d = makeDefaults()
        let s = SettingsStore(defaults: d)
        XCTAssertNil(s.noiseBaselineDb, "Automatic by default")
        XCTAssertEqual(SettingsStore.defaultNoiseBaselineDb, nil)
        XCTAssertNil(s.ffi.noiseBaselineDb)
        s.noiseBaselineDb = -52
        XCTAssertEqual(s.ffi.noiseBaselineDb, -52)
        XCTAssertEqual(SettingsStore(defaults: d).noiseBaselineDb, -52, "persisted")
        s.noiseBaselineDb = nil
        XCTAssertNil(SettingsStore(defaults: d).noiseBaselineDb, "choosing Automatic is persisted too")
        XCTAssertNil(s.ffi.noiseBaselineDb)
    }

    func testBaselineChangeReachesTheCore() {
        let core = FakeCore()
        let model = AppModel(settings: SettingsStore(defaults: makeDefaults()))
        model.attach(core: core)
        model.settings.noiseBaselineDb = -47
        XCTAssertEqual(core.lastSettings?.noiseBaselineDb, -47)
    }

    // MARK: measuring

    func testMeasuredBaselineIsThe95thPercentilePlus3dBRounded() {
        // 100 readings: -60…-41 five times each. The 95th percentile is -42.
        let levels = (0..<100).map { Float(-60 + $0 % 20) }
        XCTAssertEqual(NoiseMeasurement.baseline(from: levels), -39)
        XCTAssertEqual(NoiseMeasurement.baseline(from: [-55.4]), -52, "rounded to whole dB")
        XCTAssertEqual(NoiseMeasurement.baseline(from: [-50.6, -50.6]), -48)
        XCTAssertNil(NoiseMeasurement.baseline(from: []))
        XCTAssertEqual(NoiseMeasurement.baseline(from: [-120, -120]), -90, "kept within the slider range")
        XCTAssertEqual(NoiseMeasurement.baseline(from: [-10]), -30)
        XCTAssertNil(NoiseMeasurement.baseline(from: [.nan]))
    }

    func testMeasuringSamplesFiveSecondsOfReadingsThenSetsTheBaseline() {
        var current: FfiMicMeter? = reading(level: -58, floor: -60)
        var enabled: [Bool] = []
        let meter = NoiseMeterModel()
        var result: Double?? = .none
        meter.onMeasured = { result = .some($0) }
        meter.start(read: { current }, enable: { enabled.append($0) }, timer: false)
        XCTAssertEqual(enabled, [true])
        let t0 = Date(timeIntervalSince1970: 1_000)
        meter.startMeasuring(now: t0)
        XCTAssertEqual(meter.countdown, 5)
        for i in 1...99 { // 20 Hz for 4.95 s
            current = reading(level: i % 10 == 0 ? -50 : -58, floor: -60)
            meter.tick(now: t0.addingTimeInterval(Double(i) * 0.05))
        }
        XCTAssertEqual(meter.countdown, 1)
        XCTAssertNil(result ?? nil, "still measuring")
        meter.tick(now: t0.addingTimeInterval(5.0))
        XCTAssertNil(meter.countdown)
        XCTAssertEqual(result, .some(-47), "p95 of the samples (-50) + 3 dB")
        meter.stop()
        XCTAssertEqual(enabled, [true, false])
        XCTAssertNil(meter.reading)
    }

    func testMeasuringWithoutReadingsReportsNothing() {
        let meter = NoiseMeterModel()
        var result: Double?? = .none
        meter.onMeasured = { result = .some($0) }
        meter.start(read: { nil }, enable: { _ in }, timer: false)
        let t0 = Date(timeIntervalSince1970: 0)
        meter.startMeasuring(now: t0)
        meter.tick(now: t0.addingTimeInterval(6))
        XCTAssertEqual(result, .some(nil))
    }

    func testMeterModelDrivesTheCoreMeter() {
        let core = FakeCore()
        let model = AppModel(settings: SettingsStore(defaults: makeDefaults()))
        model.attach(core: core)
        core.meter = reading(level: -40, floor: -55)
        let meter = NoiseMeterModel()
        meter.start(model: model, timer: false)
        XCTAssertEqual(core.calls.last, "meter:true")
        meter.tick(now: Date())
        XCTAssertEqual(meter.reading, core.meter)
        meter.stop()
        XCTAssertEqual(core.calls.last, "meter:false")
    }

    // MARK: presentation

    func testAccessibilityValue() {
        XCTAssertEqual(NoiseMeterBar.accessibilityValue(reading(level: -52, floor: -58)),
                       "Level −52 dB, baseline −58 dB, 6 dB above")
        XCTAssertEqual(NoiseMeterBar.accessibilityValue(reading(level: -61.6, floor: -58)),
                       "Level −62 dB, baseline −58 dB, background, ignored")
        XCTAssertEqual(NoiseMeterBar.accessibilityValue(reading(level: -30, floor: -58, speech: true)),
                       "Level −30 dB, baseline −58 dB, 28 dB above, speech")
        XCTAssertEqual(NoiseMeterBar.accessibilityValue(nil), "No reading")
    }

    func testReadout() {
        XCTAssertEqual(NoiseMeterBar.readout(reading(level: -46, floor: -58)), "Perceived: +12 dB above baseline")
        XCTAssertEqual(NoiseMeterBar.readout(reading(level: -30, floor: -58, speech: true)), "Speech: +28 dB above baseline")
        XCTAssertEqual(NoiseMeterBar.readout(reading(level: -60, floor: -58)), "Background (ignored)")
        XCTAssertEqual(NoiseMeterBar.readout(reading(level: -57.8, floor: -58)), "Background (ignored)", "under half a dB")
    }

    func testMeterScaleMapsTheRangeAndClamps() {
        XCTAssertEqual(NoiseMeterBar.fraction(-90), 0)
        XCTAssertEqual(NoiseMeterBar.fraction(-20), 1)
        XCTAssertEqual(NoiseMeterBar.fraction(-55), 0.5, accuracy: 1e-9)
        XCTAssertEqual(NoiseMeterBar.fraction(-120), 0)
        XCTAssertEqual(NoiseMeterBar.fraction(0), 1)
        XCTAssertEqual(NoiseMeterBar.fraction(.nan), 0)
    }
}
