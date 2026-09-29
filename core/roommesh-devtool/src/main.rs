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

options:
  --force           run a loopback even while the RoomMesh app looks live
  -h, --help        show this help

The loopbacks drive the shared memory directly, so quit RoomMesh first; they refuse to run
while the app's heartbeat is fresh or a client is capturing from RoomMesh Microphone.
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
fn cmd_noise_probe() -> Outcome {
    use roommesh_core::audio::device_io::{DeviceHost, DeviceSelector};
    use roommesh_core::dsp::aec::{EchoCanceller, WebRtcAec};
    use roommesh_core::dsp::level::measure;
    use roommesh_core::dsp::vad::Vad;
    use roommesh_core::engine::uplink::FrameAssembler;

    const WARMUP_S: f32 = 2.0;
    const QUIET_S: f32 = 15.0;
    const TALK_S: f32 = 10.0;
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
    println!(
        "Stay QUIET for {:.0} s (normal room noise is fine)...",
        WARMUP_S + QUIET_S
    );
    let mut prompted_talk = false;
    let start = Instant::now();
    while start.elapsed().as_secs_f32() < WARMUP_S + QUIET_S + TALK_S {
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
            } else if t < WARMUP_S + QUIET_S {
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
        if !prompted_talk && start.elapsed().as_secs_f32() >= WARMUP_S + QUIET_S {
            prompted_talk = true;
            println!("Now TALK normally for {TALK_S:.0} s (from where you'd sit in a meeting)...");
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
    quiet.report("QUIET");
    talk.report("TALKING");
    Ok(true)
}

fn usage_error(msg: &str) -> ExitCode {
    eprintln!("error: {msg}\n\n{USAGE}");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let mut force = false;
    let mut cmd: Option<String> = None;
    for a in std::env::args().skip(1) {
        match a.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "--force" => force = true,
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
        "noise-probe" => cmd_noise_probe(),
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
