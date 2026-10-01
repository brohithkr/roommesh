//! Developer diagnostics for the RoomMesh driver and audio devices.
//!
//! Exit codes: 0 = PASS / success, 1 = FAIL or error, 2 = usage error.
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use roommesh_core::audio::device_io::list_devices;
use roommesh_core::audio::shared_layout::SHM_NAME;
use roommesh_core::audio::virtual_device::{MicWriter, SharedRegion, SpeakerReader};
use roommesh_core::time::now_ns;
use std::process::ExitCode;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const USAGE: &str = "\
usage: roommesh-devtool [--force] <command>

commands:
  devices           list the physical audio devices the app would use (RoomMesh devices excluded)
  shm-status        print the driver's shared-memory header (heartbeats, IO clients, liveness)
  shm-selftest      in-process ring round trip; no driver needed
  mic-loopback      write a tone into the mic ring and capture it from \"RoomMesh Microphone\"
  speaker-loopback  play a tone into \"RoomMesh Speaker\" and read it back from the speaker ring
  noise-probe       measure this room through the default microphone and the app's processing
                    (echo canceller + noise suppression + voice detection): 15 s quiet, 10 s talking
  health-probe      run the app's audio engine as coordinator + room speaker on this Mac's
                    devices (meeting audio: a quiet tone into \"RoomMesh Speaker\"; synthetic
                    remote mics), then print its audio health counters

options:
  --force           run a loopback even while the RoomMesh app looks live
  --talk            noise-probe: skip the quiet part and measure talking only (start right away)
  --quiet           noise-probe: measure the quiet part only
  --seconds N       noise-probe: length of each measured part (default: 15 quiet, 10 talking);
                    health-probe: how long to measure (default 60)
  --mics N          health-probe: synthetic remote mics to process (default 3, at most 16)
  --load N          health-probe: N busy threads competing for the CPU (default 0)
  --no-realtime     health-probe: run the audio thread at normal priority
  --loud            health-probe: an audible tone instead of a very quiet one
  -h, --help        show this help

The loopbacks and health-probe drive the shared memory directly, so quit RoomMesh first; they
refuse to run while the app's heartbeat is fresh or a client is capturing from RoomMesh Microphone.
health-probe plays the tone on this Mac's default output (the room speaker).
mic-loopback and noise-probe need microphone permission: run them from Terminal.app.
exit status: 0 = PASS, 1 = FAIL or error, 2 = usage error";

/// Liveness window for the app heartbeat (same threshold the driver uses for its own).
const LIVE_NS: u64 = 2_000_000_000;

/// Energy (sum of squares) a loopback must exceed to PASS.
const PASS_ENERGY: f64 = 100.0;

/// `Ok(true)` = PASS / success, `Ok(false)` = FAIL, `Err` = could not run the check.
type Outcome = Result<bool, String>;

/// A 440 Hz test tone sample at 48 kHz for sample index `i`.
fn tone(i: u64) -> f32 {
    0.25 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48_000.0).sin()
}

/// Locks `m`, ignoring poisoning: the guarded values are plain counters.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Finds a device by exact name directly through cpal, bypassing `device_io`'s exclusion of
/// RoomMesh virtual devices — the whole point of the loopback checks is to talk to them.
fn find(name: &str, input: bool) -> Result<cpal::Device, String> {
    let host = cpal::default_host();
    let mut it: Box<dyn Iterator<Item = cpal::Device>> = if input {
        Box::new(
            host.input_devices()
                .map_err(|e| format!("cannot enumerate input devices: {e}"))?,
        )
    } else {
        Box::new(
            host.output_devices()
                .map_err(|e| format!("cannot enumerate output devices: {e}"))?,
        )
    };
    it.find(|d| d.description().is_ok_and(|x| x.name() == name))
        .ok_or_else(|| format!("device '{name}' not found — is the driver installed?"))
}

fn open() -> Result<Arc<SharedRegion>, String> {
    SharedRegion::open(SHM_NAME)
        .map(Arc::new)
        .map_err(|e| format!("cannot open {SHM_NAME}: {e}"))
}

/// Formats a heartbeat's age, or "none" when it has never been written (or was cleared).
fn age(now: u64, heartbeat_ns: u64) -> String {
    if heartbeat_ns == 0 {
        "none".into()
    } else {
        format!("{} ms", now.saturating_sub(heartbeat_ns) / 1_000_000)
    }
}

/// Refuses to run a loopback while RoomMesh (or a meeting app) is using the shared memory:
/// the loopback would fight the app over the mic ring and both results would be garbage.
fn ensure_app_not_live(r: &SharedRegion, force: bool) -> Result<(), String> {
    let h = r.header();
    let now = now_ns();
    let hb = h.app_heartbeat_ns.load(Ordering::Relaxed);
    let clients = h.mic_clients.load(Ordering::Relaxed);
    let reason = if hb != 0 && now.saturating_sub(hb) < LIVE_NS {
        format!("the app heartbeat is fresh ({} old)", age(now, hb))
    } else if clients > 0 {
        format!("{clients} client(s) are capturing from RoomMesh Microphone")
    } else {
        return Ok(());
    };
    if force {
        eprintln!("warning: {reason}; continuing because of --force");
        Ok(())
    } else {
        Err(format!(
            "RoomMesh looks live: {reason}. Quit RoomMesh first (or pass --force)"
        ))
    }
}

fn cmd_devices() -> Outcome {
    for d in list_devices() {
        println!(
            "{:<40} in={} out={} default={}",
            d.name, d.is_input, d.is_output, d.is_default
        );
    }
    Ok(true)
}

fn cmd_shm_status() -> Outcome {
    let r = open()?;
    let h = r.header();
    println!(
        "magic={:#x} version={} rate={} generation={}",
        h.magic,
        h.version,
        h.sample_rate,
        h.generation.load(Ordering::Relaxed)
    );
    let now = now_ns();
    println!(
        "driver heartbeat age={}, app heartbeat age={}",
        age(now, h.driver_heartbeat_ns.load(Ordering::Relaxed)),
        age(now, h.app_heartbeat_ns.load(Ordering::Relaxed))
    );
    println!(
        "mic io active={} speaker io active={}",
        h.mic_clients.load(Ordering::Relaxed),
        h.speaker_clients.load(Ordering::Relaxed)
    );
    println!("driver alive: {}", r.driver_alive(now_ns()));
    Ok(true)
}

fn cmd_shm_selftest() -> Outcome {
    let r = Arc::new(
        SharedRegion::create_for_test().map_err(|e| format!("cannot create test region: {e}"))?,
    );
    MicWriter::new(r.clone()).write(&[0.5; 480], now_ns());
    let mut out = vec![0.0; 480];
    let n = r.test_driver_read_mic(0, &mut out);
    let ok = n == 480 && out.iter().all(|&v| v == 0.5);
    if ok {
        println!("PASS shm-selftest");
    } else {
        let bad = out.iter().filter(|&&v| v != 0.5).count();
        println!("FAIL shm-selftest: read {n}/480 frames, {bad} samples differ from 0.5");
    }
    Ok(ok)
}

#[derive(Default)]
struct Capture {
    callbacks: u64,
    samples: u64,
    nonzero: u64,
    energy: f64,
}

fn cmd_mic_loopback(force: bool) -> Outcome {
    let r = open()?;
    ensure_app_not_live(&r, force)?;
    let writer = MicWriter::new(r);
    let cap = Arc::new(Mutex::new(Capture::default()));
    let cap2 = cap.clone();
    let dev = find("RoomMesh Microphone", true)?;
    let cfg: cpal::StreamConfig = dev
        .default_input_config()
        .map_err(|e| format!("RoomMesh Microphone has no default input config: {e}"))?
        .config();
    let s = dev
        .build_input_stream::<f32, _, _>(
            cfg,
            move |d: &[f32], _: &cpal::InputCallbackInfo| {
                let mut c = lock(&cap2);
                c.callbacks += 1;
                c.samples += d.len() as u64;
                c.nonzero += d.iter().filter(|&&v| v != 0.0).count() as u64;
                c.energy += d.iter().map(|v| (v * v) as f64).sum::<f64>();
            },
            |e| eprintln!("input stream error: {e}"),
            None,
        )
        .map_err(|e| format!("cannot open an input stream on RoomMesh Microphone: {e}"))?;
    s.play()
        .map_err(|e| format!("cannot start the RoomMesh Microphone stream: {e}"))?;
    let start = Instant::now();
    let mut written = 0u64;
    while start.elapsed() < Duration::from_secs(3) {
        let due = start.elapsed().as_micros() as u64 * 48 / 1000 + 480;
        while written < due {
            let f: Vec<f32> = (0..480).map(|k| tone(written + k)).collect();
            writer.write(&f, now_ns());
            written += 480;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(s);
    let c = lock(&cap);
    let e = c.energy;
    if c.callbacks == 0 {
        println!("FAIL mic-loopback energy={e:.1}: no input callbacks received");
        return Ok(false);
    }
    if c.nonzero == 0 {
        println!(
            "FAIL mic-loopback energy={e:.1}: {} callbacks, {} samples, all exactly 0",
            c.callbacks, c.samples
        );
        println!(
            "hint: bit-exact silence is what macOS TCC feeds a process without microphone \
             permission; run from Terminal.app with microphone permission"
        );
        return Ok(false);
    }
    let pass = e > PASS_ENERGY;
    println!(
        "{} mic-loopback energy={e:.1} ({} callbacks, {} samples)",
        if pass { "PASS" } else { "FAIL" },
        c.callbacks,
        c.samples
    );
    Ok(pass)
}

fn cmd_speaker_loopback(force: bool) -> Outcome {
    let r = open()?;
    ensure_app_not_live(&r, force)?;
    let mut reader = SpeakerReader::new(r);
    let dev = find("RoomMesh Speaker", false)?;
    let cfg: cpal::StreamConfig = dev
        .default_output_config()
        .map_err(|e| format!("RoomMesh Speaker has no default output config: {e}"))?
        .config();
    let ch = cfg.channels as usize;
    let mut i = 0u64;
    let s = dev
        .build_output_stream::<f32, _, _>(
            cfg,
            move |d: &mut [f32], _: &cpal::OutputCallbackInfo| {
                for f in d.chunks_mut(ch) {
                    f.fill(tone(i));
                    i += 1;
                }
            },
            |e| eprintln!("output stream error: {e}"),
            None,
        )
        .map_err(|e| format!("cannot open an output stream on RoomMesh Speaker: {e}"))?;
    s.play()
        .map_err(|e| format!("cannot start the RoomMesh Speaker stream: {e}"))?;
    reader.read(1 << 16); // position the cursor at the live edge
                          // Drain continuously rather than sleeping and bulk-draining at the end: the ring holds
                          // ~680ms, so a single sleep(2s) then one read() would fall behind by more than that and
                          // trip the reader's overrun recovery, discarding almost everything it was meant to measure.
    let mut energy = 0.0f64;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        while let Some(chunk) = reader.read(1 << 16) {
            energy += chunk
                .samples
                .iter()
                .map(|v| (*v as f64) * (*v as f64))
                .sum::<f64>();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    while let Some(chunk) = reader.read(1 << 16) {
        energy += chunk
            .samples
            .iter()
            .map(|v| (*v as f64) * (*v as f64))
            .sum::<f64>();
    }
    let pass = energy > PASS_ENERGY;
    println!(
        "{} speaker-loopback energy={energy:.1}",
        if pass { "PASS" } else { "FAIL" }
    );
    Ok(pass)
}

/// Level percentiles of `v` (dB), as "p5 / p50 / p95".
fn percentiles(v: &mut [f32]) -> String {
    if v.is_empty() {
        return "n/a".into();
    }
    v.sort_by(|a, b| a.total_cmp(b));
    let at = |q: f32| v[((v.len() - 1) as f32 * q) as usize];
    format!(
        "p5 {:.1} / p50 {:.1} / p95 {:.1} dB",
        at(0.05),
        at(0.5),
        at(0.95)
    )
}

#[derive(Default)]
struct PhaseStats {
    raw_db: Vec<f32>,
    processed_db: Vec<f32>,
    floor_db: Vec<f32>,
    snr_db: Vec<f32>,
    speech_frames: usize,
    frames: usize,
}

impl PhaseStats {
    fn report(&mut self, name: &str) {
        println!("{name} ({} frames of 10 ms):", self.frames);
        println!("  raw mic level       {}", percentiles(&mut self.raw_db));
        println!(
            "  after processing    {}",
            percentiles(&mut self.processed_db)
        );
        println!("  VAD noise floor     {}", percentiles(&mut self.floor_db));
        println!("  VAD SNR             {}", percentiles(&mut self.snr_db));
        println!(
            "  judged speech       {:.1}% of frames",
            100.0 * self.speech_frames as f32 / self.frames.max(1) as f32
        );
    }
}

/// Captures the default microphone and runs it through the same chain the coordinator uses
/// for every mic (WebRTC AEC with noise suppression, fed silence as the far-end reference, then
/// the VAD), reporting levels for a quiet phase and a talking phase.
#[derive(Clone, Copy, PartialEq)]
enum ProbeMode {
    Both,
    QuietOnly,
    TalkOnly,
}

fn cmd_noise_probe(mode: ProbeMode, seconds: Option<f32>) -> Outcome {
    use roommesh_core::audio::device_io::{DeviceHost, DeviceSelector};
    use roommesh_core::dsp::aec::{EchoCanceller, WebRtcAec};
    use roommesh_core::dsp::level::measure;
    use roommesh_core::dsp::vad::Vad;
    use roommesh_core::engine::uplink::FrameAssembler;

    // The first second only warms up the echo canceller and the VAD; it isn't measured.
    const WARMUP_S: f32 = 1.0;
    let quiet_s = match mode {
        ProbeMode::TalkOnly => 0.0,
        _ => seconds.unwrap_or(15.0),
    };
    let talk_s = match mode {
        ProbeMode::QuietOnly => 0.0,
        _ => seconds.unwrap_or(10.0),
    };
    let host = DeviceHost::spawn();
    let mut cap = host
        .start_capture(DeviceSelector::Default)
        .map_err(|e| format!("cannot open the default microphone: {e}"))?;
    println!("microphone: {} ({} Hz)", cap.device_name, cap.sample_rate);
    let mut assembler = FrameAssembler::new(cap.sample_rate);
    let mut aec = WebRtcAec::new(true).map_err(|e| format!("echo canceller: {e}"))?;
    let mut vad = Vad::new();
    let reference = [0.0f32; 480];
    let (mut quiet, mut talk) = (PhaseStats::default(), PhaseStats::default());
    let mut first_frames = Vec::new();
    let (mut frames, mut all_zero) = (0usize, true);
    let talk_prompt = format!(
        "TALK normally for {:.0} s (from where you'd sit in a meeting)...",
        WARMUP_S + talk_s
    );
    if quiet_s > 0.0 {
        println!(
            "Stay QUIET for {:.0} s: don't type or touch the Mac (normal room noise is fine)...",
            WARMUP_S + quiet_s
        );
    } else {
        println!("Start talking now. {talk_prompt}");
    }
    let mut prompted_talk = quiet_s == 0.0;
    let start = Instant::now();
    while start.elapsed().as_secs_f32() < WARMUP_S + quiet_s + talk_s {
        while let Ok(b) = cap.blocks.pop() {
            assembler.push(b.first_frame, b.capture_ns, &b.samples[..b.len as usize]);
        }
        while let Some(frame) = assembler.pop_frame() {
            let t = frames as f32 * 0.01;
            frames += 1;
            let mut x = frame.samples;
            all_zero &= x.iter().all(|v| *v == 0.0);
            let raw = measure(&x).rms_db;
            aec.process(&reference, &mut x);
            let processed = measure(&x).rms_db;
            let r = vad.process(&x);
            if first_frames.len() < 5 {
                first_frames.push(processed);
            }
            let phase = if t < WARMUP_S {
                continue;
            } else if t < WARMUP_S + quiet_s {
                &mut quiet
            } else {
                &mut talk
            };
            phase.frames += 1;
            phase.raw_db.push(raw);
            phase.processed_db.push(processed);
            phase.floor_db.push(r.noise_floor_db);
            phase.snr_db.push(r.snr_db);
            phase.speech_frames += r.is_speech as usize;
        }
        if !prompted_talk && talk_s > 0.0 && start.elapsed().as_secs_f32() >= WARMUP_S + quiet_s {
            prompted_talk = true;
            println!("Now {talk_prompt}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    host.stop_capture();
    if frames == 0 {
        println!("FAIL noise-probe: no audio frames captured");
        return Ok(false);
    }
    if all_zero {
        println!("FAIL noise-probe: every sample was exactly 0");
        println!("hint: that is what macOS feeds a process without microphone permission; run from Terminal.app");
        return Ok(false);
    }
    println!();
    println!(
        "first processed frames: {}",
        first_frames
            .iter()
            .map(|d| format!("{d:.1}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    if quiet_s > 0.0 {
        quiet.report("QUIET");
    }
    if talk_s > 0.0 {
        talk.report("TALKING");
    }
    Ok(true)
}

struct HealthProbe {
    seconds: f32,
    mics: u64,
    load: usize,
    realtime: bool,
    loud: bool,
}

/// Runs the app's audio runtime (real devices, the driver's shared memory) as the coordinator
/// and room speaker of a one-Mac room, with meeting audio played into RoomMesh Speaker and
/// `mics` synthetic remote mics, and prints where audio was lost.
fn cmd_health_probe(force: bool, p: HealthProbe) -> Outcome {
    use roommesh_core::audio::frames::AudioFrame;
    use roommesh_core::engine::runtime::{AudioRuntime, AudioSettings, RuntimeMsg, SystemAudio};
    use roommesh_core::engine::uplink::MicUplink;
    use roommesh_core::ids::{Epoch, PeerId, RoomId};
    use roommesh_core::network::loopback::LoopbackNetwork;
    use roommesh_core::room::events::LocalRoles;
    use std::sync::atomic::AtomicBool;

    let r = open()?;
    ensure_app_not_live(&r, force)?;
    drop(r);
    if !p.realtime {
        // Read by the audio thread when it starts (engine::realtime).
        std::env::set_var("ROOMMESH_NO_REALTIME", "1");
    }
    // Meeting audio: a tone into RoomMesh Speaker, as a meeting app would play it.
    let dev = find("RoomMesh Speaker", false)?;
    let cfg: cpal::StreamConfig = dev
        .default_output_config()
        .map_err(|e| format!("RoomMesh Speaker has no default output config: {e}"))?
        .config();
    let ch = cfg.channels as usize;
    let gain = if p.loud { 1.0 } else { 0.02 }; // the tone is 0.25 peak: -12 or -46 dBFS
    let mut i = 0u64;
    let meeting = dev
        .build_output_stream::<f32, _, _>(
            cfg,
            move |d: &mut [f32], _: &cpal::OutputCallbackInfo| {
                for f in d.chunks_mut(ch) {
                    f.fill(gain * tone(i));
                    i += 1;
                }
            },
            |e| eprintln!("meeting stream error: {e}"),
            None,
        )
        .map_err(|e| format!("cannot open an output stream on RoomMesh Speaker: {e}"))?;
    meeting
        .play()
        .map_err(|e| format!("cannot start the RoomMesh Speaker stream: {e}"))?;

    let local = PeerId(1);
    let net = LoopbackNetwork::new();
    let (tsink, _transport_events) = crossbeam_channel::unbounded();
    let (ev_tx, ev_rx) = crossbeam_channel::unbounded();
    let rt = AudioRuntime::spawn(
        local,
        Box::new(SystemAudio::new()),
        net.transport(local, tsink),
        Default::default(),
        ev_tx,
        AudioSettings::default(),
    );
    let remotes: Vec<PeerId> = (2..2 + p.mics).map(PeerId).collect();
    rt.send(RuntimeMsg::SetEnabled(true));
    rt.send(RuntimeMsg::Roles(LocalRoles {
        room_id: Some(RoomId(1)),
        epoch: Epoch(1),
        coordinator: Some(local),
        speaker: Some(local),
        is_coordinator: true,
        is_speaker: true,
        mic_enabled: false, // this Mac's mic stays closed (no microphone permission needed)
        enabled_mics: remotes.clone(),
        noise_baseline_db: None,
        members: std::iter::once(local)
            .chain(remotes.iter().copied())
            .collect(),
    }));

    let stop = Arc::new(AtomicBool::new(false));
    // Remote mics: one 10 ms packet per mic every 10 ms, captured 15 ms before it arrives.
    let feeder = {
        let (tx, stop) = (rt.sender(), stop.clone());
        let remotes = remotes.clone();
        std::thread::spawn(move || {
            let mut ups: Vec<MicUplink> = remotes
                .iter()
                .map(|p| MicUplink::new(*p, 48_000).expect("opus encoder"))
                .collect();
            let start = Instant::now();
            let mut k = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let now = now_ns();
                for (m, up) in ups.iter_mut().enumerate() {
                    let n0 = k * 480;
                    let f = AudioFrame {
                        sample_index: n0,
                        timestamp_ns: now - 15_000_000,
                        samples: (n0..n0 + 480)
                            .map(|n| 0.1 * tone(n * (m as u64 + 2)))
                            .collect(),
                    };
                    if let Ok((header, payload)) = up.packetize(&f, Epoch(1), f.timestamp_ns) {
                        tx.send(RuntimeMsg::Packet {
                            header,
                            payload,
                            arrival_ns: now,
                        });
                    }
                }
                k += 1;
                let next = start + Duration::from_millis(10 * k);
                std::thread::sleep(next.saturating_duration_since(Instant::now()));
            }
        })
    };
    let load: Vec<_> = (0..p.load)
        .map(|_| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut x = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    for _ in 0..10_000 {
                        x = std::hint::black_box(x.wrapping_mul(6_364_136_223_846_793_005) + 1);
                    }
                }
            })
        })
        .collect();

    // Let the devices open and the queues settle, then count from zero.
    std::thread::sleep(Duration::from_secs(3));
    rt.send(RuntimeMsg::ResetHealth);
    std::thread::sleep(Duration::from_secs_f32(p.seconds) + Duration::from_millis(1_100));
    let h = rt.shared().health.lock().clone();
    stop.store(true, Ordering::Relaxed);
    let _ = feeder.join();
    for t in load {
        let _ = t.join();
    }
    rt.shutdown();
    drop(meeting);

    let ms = |ns: u64| ns as f64 / 1e6;
    let c = &h.since_start;
    println!(
        "health-probe: {:.0} s, {} remote mic(s), {} load thread(s); audio thread: {}, \
         demotions {}",
        p.seconds,
        p.mics,
        p.load,
        h.realtime.label(),
        h.demotions
    );
    println!(
        "  wakes {}: worst delay {:.2} ms, p99 {:.2} ms",
        c.wakes,
        ms(c.wake_worst_ns),
        ms(c.wake_p99_ns)
    );
    println!(
        "  speaker underruns {} ({:.1} ms); speaker gaps {:.1} ms; late meeting frames {}; \
         meeting restarts {}; room-mic silence slots {}",
        c.speaker_underruns,
        ms(c.speaker_underrun_ns),
        ms(c.speaker_missing_ns),
        c.farend_late_frames,
        c.farend_resets,
        c.mic_silence_slots
    );
    let missing: Vec<String> = c
        .mic_missing_ns
        .iter()
        .map(|(p, ns)| format!("{}={:.1}ms", p.to_hex(), ms(*ns)))
        .collect();
    println!(
        "  missing mic audio: {}",
        if missing.is_empty() {
            "none".into()
        } else {
            missing.join(", ")
        }
    );
    for e in ev_rx.try_iter() {
        println!("  event: {e:?}");
    }
    Ok(true)
}

fn usage_error(msg: &str) -> ExitCode {
    eprintln!("error: {msg}\n\n{USAGE}");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let mut force = false;
    let mut mode = ProbeMode::Both;
    let mut seconds: Option<f32> = None;
    let mut mics = 3u64;
    let mut load = 0usize;
    let mut realtime = true;
    let mut loud = false;
    let mut cmd: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "--force" => force = true,
            "--talk" => mode = ProbeMode::TalkOnly,
            "--quiet" => mode = ProbeMode::QuietOnly,
            "--seconds" => match args.next().and_then(|v| v.parse::<f32>().ok()) {
                Some(v) if v > 0.0 && v <= 600.0 => seconds = Some(v),
                _ => return usage_error("--seconds needs a number of seconds (1-600)"),
            },
            "--mics" => match args.next().and_then(|v| v.parse::<u64>().ok()) {
                Some(v) if v <= 16 => mics = v,
                _ => return usage_error("--mics needs a number of mics (0-16)"),
            },
            "--load" => match args.next().and_then(|v| v.parse::<usize>().ok()) {
                Some(v) if v <= 64 => load = v,
                _ => return usage_error("--load needs a number of threads (0-64)"),
            },
            "--no-realtime" => realtime = false,
            "--loud" => loud = true,
            s if s.starts_with('-') => return usage_error(&format!("unknown option '{s}'")),
            s if cmd.is_none() => cmd = Some(s.to_string()),
            s => return usage_error(&format!("unexpected argument '{s}'")),
        }
    }
    let Some(cmd) = cmd else {
        return usage_error("no command given");
    };
    let outcome = match cmd.as_str() {
        "devices" => cmd_devices(),
        "shm-status" => cmd_shm_status(),
        "shm-selftest" => cmd_shm_selftest(),
        "mic-loopback" => cmd_mic_loopback(force),
        "speaker-loopback" => cmd_speaker_loopback(force),
        "noise-probe" => cmd_noise_probe(mode, seconds),
        "health-probe" => cmd_health_probe(
            force,
            HealthProbe {
                seconds: seconds.unwrap_or(60.0),
                mics,
                load,
                realtime,
                loud,
            },
        ),
        other => return usage_error(&format!("unknown command '{other}'")),
    };
    match outcome {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("FAIL {cmd}: {e}");
            ExitCode::FAILURE
        }
    }
}
