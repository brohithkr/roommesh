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

    // MARK: baseline slider

    func testSliderPushesTheBaselineOnlyAfterAPauseOrWhenTheDragEnds() {
        let clock = ManualScheduler()
        var commits: [Double] = []
        let slider = BaselineSliderModel(delay: 0.3, schedule: clock.schedule)
        slider.commit = { commits.append($0) }
        slider.drag(to: -60.2)
        XCTAssertEqual(slider.displayed(committed: -58), -60, "the label follows at once, in whole dB")
        clock.advance(0.2)
        slider.drag(to: -55)
        clock.advance(0.2)
        slider.drag(to: -50.4)
        XCTAssertEqual(commits, [], "still moving")
        XCTAssertEqual(slider.displayed(committed: -58), -50)
        clock.advance(0.29)
        XCTAssertEqual(commits, [])
        clock.advance(0.02)
        XCTAssertEqual(commits, [-50], "300 ms without a change")
        XCTAssertNil(slider.draft)
        XCTAssertEqual(slider.displayed(committed: -50), -50)
        // The drag ending pushes at once, and only once.
        slider.drag(to: -45)
        slider.endDrag()
        XCTAssertEqual(commits, [-50, -45])
        clock.advance(1)
        slider.endDrag()
        XCTAssertEqual(commits, [-50, -45])
        // Moving within the same whole dB is no change.
        slider.drag(to: -45.2, committed: -45)
        clock.advance(1)
        XCTAssertEqual(commits, [-50, -45])
        // A pending value can be dropped (Automatic chosen, or a measurement set the baseline).
        slider.drag(to: -40)
        slider.cancel()
        clock.advance(1)
        XCTAssertEqual(commits, [-50, -45])
        XCTAssertNil(slider.draft)
    }

    func testOneDragSendsOneSettingsUpdate() {
        let core = FakeCore()
        let model = AppModel(settings: SettingsStore(defaults: makeDefaults()))
        model.attach(core: core)
        model.settings.noiseBaselineDb = -60
        core.calls = []
        let clock = ManualScheduler()
        let slider = BaselineSliderModel(delay: 0.3, schedule: clock.schedule)
        slider.commit = { model.settings.noiseBaselineDb = $0 }
        for (i, v) in stride(from: -60.0, through: -40.0, by: 0.5).enumerated() {
            slider.drag(to: v, committed: model.settings.noiseBaselineDb)
            clock.advance(i.isMultiple(of: 2) ? 0.05 : 0.1)
        }
        slider.endDrag()
        XCTAssertEqual(core.calls.filter { $0 == "settings" }.count, 1)
        XCTAssertEqual(core.lastSettings?.noiseBaselineDb, -40)
    }

    // MARK: meter lifetime

    func testTheMeterIsReleasedWhenTheModelGoesAway() {
        var enabled: [Bool] = []
        do {
            let meter = NoiseMeterModel()
            meter.start(read: { nil }, enable: { enabled.append($0) }, timer: true)
            XCTAssertEqual(enabled, [true])
        }
        XCTAssertEqual(enabled, [true, false], "deinit stops the timer and disables the meter")
    }

    func testResigningActiveStopsTheMeterWhenSettingsIsClosed() {
        let center = NotificationCenter()
        var visible = true
        var enabled: [Bool] = []
        let meter = NoiseMeterModel(center: center)
        meter.start(read: { nil }, enable: { enabled.append($0) }, timer: false, visible: { visible })
        let resign = { center.post(name: NSApplication.didResignActiveNotification, object: nil) }
        let activate = { center.post(name: NSApplication.didBecomeActiveNotification, object: nil) }
        resign()
        XCTAssertEqual(enabled, [true], "Settings still on screen: keep metering")
        visible = false
        resign()
        XCTAssertEqual(enabled, [true, false])
        activate()
        XCTAssertEqual(enabled, [true, false], "Settings closed: stays off")
        visible = true
        activate()
        XCTAssertEqual(enabled, [true, false, true], "back on screen: resumes")
        meter.stop()
        activate()
        XCTAssertEqual(enabled, [true, false, true, false], "stopped for good")
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

/// A test clock for `Debouncer`: scheduled work runs when `advance` passes its due time.
@MainActor
final class ManualScheduler {
    private(set) var now: TimeInterval = 0
    private var tasks: [(id: Int, at: TimeInterval, run: @MainActor () -> Void)] = []
    private var nextId = 0

    func schedule(_ delay: TimeInterval, _ run: @escaping @MainActor () -> Void) -> () -> Void {
        let id = nextId
        nextId += 1
        tasks.append((id, now + delay, run))
        return { [weak self] in self?.tasks.removeAll { $0.id == id } }
    }

    func advance(_ dt: TimeInterval) {
        now += dt
        while let i = tasks.indices.filter({ tasks[$0].at <= now + 1e-9 }).min(by: { tasks[$0].at < tasks[$1].at }) {
            let t = tasks.remove(at: i)
            t.run()
        }
    }
}
