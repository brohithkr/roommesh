import AppKit
import SwiftUI

/// "Measure room noise": the custom baseline from a few seconds of quiet-room readings.
enum NoiseMeasurement {
    static let duration: TimeInterval = 5
    /// Headroom above the loudest ordinary room noise.
    static let marginDb: Double = 3
    /// The baseline slider's range (dB).
    static let range: ClosedRange<Double> = -90 ... -30

    /// The 95th percentile (nearest rank) of `levels` plus `marginDb`, rounded to whole dB and
    /// kept within `range`. `nil` without a usable reading.
    static func baseline(from levels: [Float]) -> Double? {
        let sorted = levels.filter(\.isFinite).sorted()
        guard !sorted.isEmpty else { return nil }
        let rank = Int((0.95 * Double(sorted.count)).rounded(.up))
        let p95 = Double(sorted[max(rank - 1, 0)])
        return min(max((p95 + marginDb).rounded(), range.lowerBound), range.upperBound)
    }
}

/// Runs the last value sent after `delay` without another one (or at once on `flush`).
@MainActor
final class Debouncer<Value> {
    /// Runs `work` after the delay; returns a cancel function.
    typealias Schedule = @MainActor (_ delay: TimeInterval, _ work: @escaping @MainActor () -> Void) -> () -> Void

    static func mainQueue(_ delay: TimeInterval, _ work: @escaping @MainActor () -> Void) -> () -> Void {
        let item = DispatchWorkItem { MainActor.assumeIsolated { work() } }
        DispatchQueue.main.asyncAfter(deadline: .now() + delay, execute: item)
        return { item.cancel() }
    }

    let delay: TimeInterval
    private let schedule: Schedule
    private let commit: @MainActor (Value) -> Void
    private var pending: Value?
    private var cancelTimer: (() -> Void)?

    init(delay: TimeInterval, schedule: @escaping Schedule = mainQueue, commit: @escaping @MainActor (Value) -> Void) {
        self.delay = delay
        self.schedule = schedule
        self.commit = commit
    }

    func send(_ value: Value) {
        pending = value
        cancelTimer?()
        cancelTimer = schedule(delay) { [weak self] in self?.flush() }
    }

    /// Commits the pending value now, if there is one.
    func flush() {
        cancelTimer?()
        cancelTimer = nil
        guard let v = pending else { return }
        pending = nil
        commit(v)
    }

    func cancel() {
        cancelTimer?()
        cancelTimer = nil
        pending = nil
    }
}

/// The custom-baseline slider: the label follows the drag at once, but the value is pushed to
/// the settings (and so to the core and, as an UpdateMember, to the whole room) only after
/// `delay` without a change or when the drag ends, so one drag makes one manifest revision.
@MainActor @Observable
final class BaselineSliderModel {
    /// The dragged value not pushed yet.
    private(set) var draft: Double?
    @ObservationIgnored var commit: (Double) -> Void = { _ in }
    @ObservationIgnored private var debouncer: Debouncer<Double>!

    init(delay: TimeInterval = 0.3, schedule: @escaping Debouncer<Double>.Schedule = Debouncer<Double>.mainQueue) {
        debouncer = Debouncer(delay: delay, schedule: schedule) { [weak self] v in
            guard let self else { return }
            self.draft = nil
            self.commit(v)
        }
    }

    /// What the label shows: the drag in progress, else the setting.
    func displayed(committed: Double?) -> Double? { draft ?? committed }

    /// The slider moved to `value` (rounded to whole dB); `committed` is the current setting.
    func drag(to value: Double, committed: Double? = nil) {
        let v = value.rounded()
        guard v != (draft ?? committed) else { return }
        draft = v
        debouncer.send(v)
    }

    func endDrag() { debouncer.flush() }

    /// Drops a pending value (the setting was changed some other way).
    func cancel() {
        debouncer.cancel()
        draft = nil
    }
}

/// Polls the core's live mic meter at 20 Hz while the Settings view is visible, and runs a
/// "Measure room noise" pass over those readings.
///
/// The core meter costs an extra echo canceller and keeps the mic open, so it is released not
/// only on `stop()` (the view's onDisappear) but also when the model goes away, and it is
/// suspended while the app is inactive with Settings closed.
@MainActor @Observable
final class NoiseMeterModel {
    private(set) var reading: FfiMicMeter?
    /// Whole seconds left of a running measurement (`nil`: not measuring).
    private(set) var countdown: Int?
    /// Called with the measured baseline (`nil`: no readings came in) when a measurement ends.
    @ObservationIgnored var onMeasured: ((Double?) -> Void)?

    @ObservationIgnored private var read: () -> FfiMicMeter? = { nil }
    @ObservationIgnored private var enable: (Bool) -> Void = { _ in }
    @ObservationIgnored private var visible: () -> Bool = { true }
    @ObservationIgnored private var useTimer = true
    @ObservationIgnored private var measureEnd: Date?
    @ObservationIgnored private var samples: [Float] = []
    /// `start` was called and `stop` wasn't: metering resumes when Settings is back on screen.
    @ObservationIgnored private var wanted = false
    /// The timer, the meter's release and the app-state observers; torn down by `deinit` too.
    @ObservationIgnored private let lease: MeterLease

    static let interval: TimeInterval = 0.05

    init(center: NotificationCenter = .default) {
        lease = MeterLease(center: center)
    }

    deinit {
        let lease = lease
        if Thread.isMainThread {
            MainActor.assumeIsolated { lease.release() }
        } else {
            DispatchQueue.main.async { MainActor.assumeIsolated { lease.release() } }
        }
    }

    func start(model: AppModel, timer: Bool = true, visible: @escaping () -> Bool = { true }) {
        start(read: { [weak model] in model?.micMeter() }, enable: { [weak model] in model?.setMicMeterEnabled($0) },
              timer: timer, visible: visible)
    }

    /// `timer: false` leaves ticking to the caller (tests). `visible`: whether the view is on
    /// screen (its Settings window is open).
    func start(read: @escaping () -> FfiMicMeter?, enable: @escaping (Bool) -> Void, timer: Bool = true,
               visible: @escaping () -> Bool = { true }) {
        stop()
        self.read = read
        self.enable = enable
        self.visible = visible
        useTimer = timer
        wanted = true
        lease.observe(NSApplication.didResignActiveNotification) { [weak self] in
            guard let self, !self.visible() else { return }
            self.suspend()
        }
        lease.observe(NSApplication.didBecomeActiveNotification) { [weak self] in
            guard let self, self.wanted, self.visible() else { return }
            self.resume()
        }
        resume()
    }

    func stop() {
        wanted = false
        lease.stopObserving()
        suspend()
    }

    private func resume() {
        guard !lease.isRunning else { return }
        enable(true)
        var timer: Timer?
        if useTimer {
            // Common modes: keeps refreshing while a slider is being dragged.
            let t = Timer(timeInterval: Self.interval, repeats: true) { [weak self] _ in
                MainActor.assumeIsolated { self?.tick(now: Date()) }
            }
            RunLoop.main.add(t, forMode: .common)
            timer = t
        }
        lease.running(timer: timer, release: enable)
    }

    private func suspend() {
        guard lease.isRunning else { return }
        lease.release(observers: false)
        reading = nil
        measureEnd = nil
        countdown = nil
        samples = []
    }

    func tick(now: Date) {
        let r = read()
        if r != reading { reading = r }
        guard let end = measureEnd else { return }
        if let r { samples.append(r.levelDb) }
        if now >= end {
            measureEnd = nil
            countdown = nil
            let result = NoiseMeasurement.baseline(from: samples)
            samples = []
            onMeasured?(result)
        } else {
            let left = Int(end.timeIntervalSince(now).rounded(.up))
            if left != countdown { countdown = left }
        }
    }

    func startMeasuring(now: Date = Date()) {
        samples = []
        measureEnd = now.addingTimeInterval(NoiseMeasurement.duration)
        countdown = Int(NoiseMeasurement.duration)
    }
}

/// The live meter: processed level, peak tick and the effective baseline marker on a
/// -90…-20 dB scale. Above the marker the bar is highlighted (accent while speech, orange for
/// "heard but not speech"); below it it is grey ("ignored as background").
struct NoiseMeterBar: View {
    let reading: FfiMicMeter?

    static let range: ClosedRange<Float> = -90 ... -20
    static let ticks: [Float] = [-90, -70, -50, -30]

    /// Position of `db` on the scale, 0…1.
    static func fraction(_ db: Float) -> Double {
        guard db.isFinite else { return 0 }
        let f = (db - range.lowerBound) / (range.upperBound - range.lowerBound)
        return Double(min(max(f, 0), 1))
    }

    /// "−52 dB" (typographic minus).
    static func dbText(_ db: Float) -> String {
        let v = Int(db.rounded())
        return v < 0 ? "−\(-v) dB" : "\(v) dB"
    }

    static func isBackground(_ r: FfiMicMeter) -> Bool { r.perceivedDb < 0.5 }

    static func readout(_ r: FfiMicMeter?) -> String {
        guard let r else { return "Waiting for the microphone…" }
        if isBackground(r) { return "Background (ignored)" }
        return "\(r.isSpeech ? "Speech" : "Perceived"): +\(Int(r.perceivedDb.rounded())) dB above baseline"
    }

    static func accessibilityValue(_ r: FfiMicMeter?) -> String {
        guard let r else { return "No reading" }
        let head = "Level \(dbText(r.levelDb)), baseline \(dbText(r.floorDb))"
        if isBackground(r) { return head + ", background, ignored" }
        return head + ", \(Int(r.perceivedDb.rounded())) dB above" + (r.isSpeech ? ", speech" : "")
    }

    private let barHeight: CGFloat = 12
    private let markerOverhang: CGFloat = 4

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            GeometryReader { geo in
                let w = geo.size.width
                let x = { (db: Float) in CGFloat(Self.fraction(db)) * w }
                ZStack(alignment: .topLeading) {
                    RoundedRectangle(cornerRadius: 3)
                        .fill(Color.primary.opacity(0.08))
                        .frame(height: barHeight)
                        .offset(y: markerOverhang)
                    if let r = reading {
                        let level = x(r.levelDb), floor = x(r.floorDb)
                        // Ignored part: up to the level or the marker, whichever is lower.
                        Rectangle()
                            .fill(Color.secondary.opacity(0.45))
                            .frame(width: min(level, floor), height: barHeight)
                            .offset(y: markerOverhang)
                        if level > floor {
                            Rectangle()
                                .fill(r.isSpeech ? Color.accentColor : Color.orange)
                                .frame(width: level - floor, height: barHeight)
                                .offset(x: floor, y: markerOverhang)
                        }
                        Rectangle()
                            .fill(Color.primary.opacity(0.55))
                            .frame(width: 2, height: barHeight)
                            .offset(x: min(max(x(r.peakDb) - 1, 0), w - 2), y: markerOverhang)
                        Capsule()
                            .fill(Color.primary)
                            .frame(width: 3, height: barHeight + 2 * markerOverhang)
                            .offset(x: min(max(floor - 1.5, 0), w - 3))
                    }
                }
                .clipShape(Rectangle())
            }
            .frame(height: barHeight + 2 * markerOverhang)
            GeometryReader { geo in
                ForEach(Self.ticks, id: \.self) { t in
                    Text(Self.dbText(t).replacingOccurrences(of: " dB", with: ""))
                        .font(.caption2.monospacedDigit())
                        .foregroundStyle(.secondary)
                        .fixedSize()
                        .position(x: min(max(CGFloat(Self.fraction(t)) * geo.size.width, 10), geo.size.width - 10), y: 6)
                }
            }
            .frame(height: 12)
            .accessibilityHidden(true)
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("Microphone level")
        .accessibilityValue(Self.accessibilityValue(reading))
    }
}

/// What `NoiseMeterModel` must undo: its timer, the core meter and its notification observers.
/// A separate object so `deinit` can release them.
@MainActor
private final class MeterLease: @unchecked Sendable {
    private let center: NotificationCenter
    private var timer: Timer?
    private var disable: ((Bool) -> Void)?
    private var observers: [NSObjectProtocol] = []

    init(center: NotificationCenter) { self.center = center }

    var isRunning: Bool { disable != nil }

    func running(timer: Timer?, release: @escaping (Bool) -> Void) {
        self.timer = timer
        disable = release
    }

    /// App activation notifications arrive on the main thread.
    func observe(_ name: Notification.Name, _ handler: @escaping @MainActor @Sendable () -> Void) {
        observers.append(center.addObserver(forName: name, object: nil, queue: nil) { _ in
            if Thread.isMainThread {
                MainActor.assumeIsolated { handler() }
            } else {
                DispatchQueue.main.async { MainActor.assumeIsolated { handler() } }
            }
        })
    }

    func stopObserving() {
        observers.forEach(center.removeObserver)
        observers = []
    }

    /// Stops the timer and disables the core meter (and, by default, stops observing).
    func release(observers: Bool = true) {
        timer?.invalidate()
        timer = nil
        disable?(false)
        disable = nil
        if observers { stopObserving() }
    }
}

/// Reports the NSWindow a view is in (nil until it is in one).
private struct WindowReader: NSViewRepresentable {
    let onWindow: (NSWindow?) -> Void
    final class Probe: NSView {
        var onWindow: (NSWindow?) -> Void = { _ in }
        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            onWindow(window)
        }
    }
    func makeNSView(context: Context) -> Probe {
        let v = Probe()
        v.onWindow = onWindow
        return v
    }
    func updateNSView(_ v: Probe, context: Context) { v.onWindow = onWindow }
}

/// Holds the Settings window weakly for `NoiseMeterModel`'s visibility check.
@MainActor
private final class WeakWindow {
    weak var window: NSWindow?
}

/// Settings › Audio › Background noise: the live meter and the Automatic / Custom baseline.
struct NoiseBaselineSection: View {
    @Environment(AppModel.self) private var model
    @State private var meter = NoiseMeterModel()
    @State private var slider = BaselineSliderModel()
    @State private var window = WeakWindow()
    @State private var measureMessage: String?

    var body: some View {
        @Bindable var settings = model.settings
        Section {
            VStack(alignment: .leading, spacing: 6) {
                NoiseMeterBar(reading: meter.reading)
                Text(NoiseMeterBar.readout(meter.reading))
                    .font(.callout.monospacedDigit())
                    .foregroundStyle(meter.reading.map { NoiseMeterBar.isBackground($0) } ?? true ? .secondary : .primary)
            }
            .padding(.vertical, 2)
            .background(WindowReader { [window] in window.window = $0 })
            Picker("Baseline", selection: customBinding) {
                Text("Automatic").tag(false)
                Text("Custom baseline").tag(true)
            }
            .pickerStyle(.segmented)
            if let baseline = slider.displayed(committed: settings.noiseBaselineDb) {
                LabeledContent {
                    HStack {
                        // 1 dB steps via the binding (`step:` would draw 60 tick marks).
                        Slider(value: sliderBinding, in: NoiseMeasurement.range,
                               onEditingChanged: { editing in if !editing { slider.endDrag() } })
                        .accessibilityValue(NoiseMeterBar.dbText(Float(baseline)))
                        Text(NoiseMeterBar.dbText(Float(baseline)))
                            .monospacedDigit()
                            .frame(minWidth: 52, alignment: .trailing)
                    }
                } label: {
                    Text("Custom baseline")
                }
            } else {
                LabeledContent("Automatic floor", value: meter.reading.map { NoiseMeterBar.dbText($0.autoFloorDb) } ?? "—")
            }
            HStack {
                Button("Measure room noise") {
                    measureMessage = nil
                    slider.endDrag()
                    meter.startMeasuring()
                }
                .disabled(meter.countdown != nil)
                if let left = meter.countdown {
                    ProgressView().controlSize(.small)
                    Text("Stay quiet… \(left) s").foregroundStyle(.secondary).monospacedDigit()
                } else if let measureMessage {
                    Text(measureMessage).foregroundStyle(.secondary)
                }
            }
        } header: {
            Text("Background noise")
        } footer: {
            Text("Sound at or below the baseline is treated as background and never selects this Mac's microphone. Measure with the room quiet.")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        .onAppear {
            meter.onMeasured = { [weak model] result in
                guard let model else { return }
                if let result {
                    slider.cancel()
                    model.settings.noiseBaselineDb = result
                    measureMessage = "Baseline set to \(NoiseMeterBar.dbText(Float(result)))"
                } else {
                    measureMessage = "No microphone readings — check the microphone and try again"
                }
            }
            slider.commit = { [weak model] v in model?.settings.noiseBaselineDb = v }
            meter.start(model: model, visible: { [window] in window.window?.isVisible == true })
        }
        .onDisappear {
            slider.endDrag()
            meter.stop()
        }
    }

    /// Automatic ↔ Custom; switching to Custom starts from the current automatic floor.
    private var customBinding: Binding<Bool> {
        Binding(
            get: { model.settings.noiseBaselineDb != nil },
            set: { custom in
                guard custom != (model.settings.noiseBaselineDb != nil) else { return }
                slider.cancel()
                if custom {
                    let start = meter.reading.map { Double($0.autoFloorDb).rounded() } ?? -60
                    model.settings.noiseBaselineDb = min(max(start, NoiseMeasurement.range.lowerBound), NoiseMeasurement.range.upperBound)
                } else {
                    model.settings.noiseBaselineDb = nil
                }
            })
    }

    private var sliderBinding: Binding<Double> {
        Binding(
            get: { slider.displayed(committed: model.settings.noiseBaselineDb) ?? -60 },
            set: { slider.drag(to: $0, committed: model.settings.noiseBaselineDb) })
    }
}
