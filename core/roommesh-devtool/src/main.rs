//! Developer diagnostics for the RoomMesh driver and audio devices.
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use roommesh_core::audio::device_io::list_devices;
use roommesh_core::audio::shared_layout::SHM_NAME;
use roommesh_core::audio::virtual_device::{MicWriter, SharedRegion, SpeakerReader};
use roommesh_core::time::now_ns;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A 440 Hz test tone sample at 48 kHz for sample index `i`.
fn tone(i: u64) -> f32 {
    0.25 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48_000.0).sin()
}

/// Finds a device by exact name directly through cpal, bypassing `device_io`'s exclusion of
/// RoomMesh virtual devices — the whole point of the loopback checks is to talk to them.
fn find(name: &str, input: bool) -> cpal::Device {
    let host = cpal::default_host();
    let mut it: Box<dyn Iterator<Item = cpal::Device>> = if input {
        Box::new(host.input_devices().expect("enumerate input devices"))
    } else {
        Box::new(host.output_devices().expect("enumerate output devices"))
    };
    it.find(|d| d.description().is_ok_and(|x| x.name() == name))
        .unwrap_or_else(|| panic!("device '{name}' not found — is the driver installed?"))
}

fn open() -> Arc<SharedRegion> {
    Arc::new(SharedRegion::open(SHM_NAME).unwrap_or_else(|e| {
        eprintln!("FAIL: {e}");
        std::process::exit(1)
    }))
}

fn cmd_devices() {
    for d in list_devices() {
        println!(
            "{:<40} in={} out={} default={}",
            d.name, d.is_input, d.is_output, d.is_default
        );
    }
}

fn cmd_shm_status() {
    let r = open();
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
        "driver heartbeat age={} ms, app heartbeat age={} ms",
        now.saturating_sub(h.driver_heartbeat_ns.load(Ordering::Relaxed)) / 1_000_000,
        now.saturating_sub(h.app_heartbeat_ns.load(Ordering::Relaxed)) / 1_000_000
    );
    println!(
        "mic io active={} speaker io active={}",
        h.mic_clients.load(Ordering::Relaxed),
        h.speaker_clients.load(Ordering::Relaxed)
    );
    println!("driver alive: {}", r.driver_alive(now_ns()));
}

fn cmd_shm_selftest() {
    let r = Arc::new(SharedRegion::create_for_test().expect("create"));
    MicWriter::new(r.clone()).write(&[0.5; 480], now_ns());
    let mut out = vec![0.0; 480];
    assert_eq!(r.test_driver_read_mic(0, &mut out), 480);
    println!("PASS shm-selftest");
}

fn cmd_mic_loopback() {
    let r = open();
    let writer = MicWriter::new(r);
    let energy = Arc::new(Mutex::new(0.0f64));
    let e2 = energy.clone();
    let dev = find("RoomMesh Microphone", true);
    let cfg: cpal::StreamConfig = dev.default_input_config().unwrap().config();
    let s = dev
        .build_input_stream::<f32, _, _>(
            cfg,
            move |d: &[f32], _: &cpal::InputCallbackInfo| {
                *e2.lock().unwrap() += d.iter().map(|v| (v * v) as f64).sum::<f64>();
            },
            |e| eprintln!("{e}"),
            None,
        )
        .unwrap();
    s.play().unwrap();
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
    let e = *energy.lock().unwrap();
    println!(
        "{} mic-loopback energy={e:.1}",
        if e > 100.0 { "PASS" } else { "FAIL" }
    );
}

fn cmd_speaker_loopback() {
    let mut reader = SpeakerReader::new(open());
    let dev = find("RoomMesh Speaker", false);
    let cfg: cpal::StreamConfig = dev.default_output_config().unwrap().config();
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
            |e| eprintln!("{e}"),
            None,
        )
        .unwrap();
    s.play().unwrap();
    reader.read(1);
    std::thread::sleep(Duration::from_secs(2));
    let e: f64 = std::iter::from_fn(|| reader.read(1 << 16))
        .flat_map(|c| c.samples)
        .map(|v| (v * v) as f64)
        .sum();
    println!(
        "{} speaker-loopback energy={e:.1}",
        if e > 100.0 { "PASS" } else { "FAIL" }
    );
}

fn main() {
    let cmd = std::env::args().nth(1).unwrap_or_default();
    match cmd.as_str() {
        "devices" => cmd_devices(),
        "shm-status" => cmd_shm_status(),
        "shm-selftest" => cmd_shm_selftest(),
        "mic-loopback" => cmd_mic_loopback(),
        "speaker-loopback" => cmd_speaker_loopback(),
        _ => eprintln!(
            "usage: roommesh-devtool devices|shm-status|shm-selftest|mic-loopback|speaker-loopback"
        ),
    }
}
