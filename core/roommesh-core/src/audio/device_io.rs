//! Physical microphone/speaker IO via cpal. Realtime callbacks only touch lock-free rings.
use crate::audio::device_clock::DeviceClock;
use crate::time::now_ns;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::{bounded, unbounded, Sender};

pub const BLOCK_FRAMES: usize = 512;

#[derive(Clone, Copy)]
pub struct CaptureBlock {
    pub first_frame: u64,
    pub capture_ns: u64,
    pub len: u16,
    pub samples: [f32; BLOCK_FRAMES],
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PlaybackReport {
    pub output_frames: u64,
    pub popped_frames: u64,
    pub play_ns: u64,
}

pub struct CaptureHandle {
    pub blocks: rtrb::Consumer<CaptureBlock>,
    pub sample_rate: u32,
    pub device_name: String,
}
pub struct PlaybackHandle {
    pub samples: rtrb::Producer<f32>,
    pub reports: rtrb::Consumer<PlaybackReport>,
    pub sample_rate: u32,
    pub device_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceSelector {
    Default,
    Name(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    pub name: String,
    pub is_input: bool,
    pub is_output: bool,
    pub is_default: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum DeviceError {
    #[error("audio device not found")]
    NotFound,
    #[error("audio device error: {0}")]
    Backend(String),
}
fn be<E: std::fmt::Display>(e: E) -> DeviceError {
    DeviceError::Backend(e.to_string())
}

pub fn is_roommesh_device(name: &str) -> bool {
    name.starts_with("RoomMesh")
}

pub fn downmix_into(interleaved: &[f32], channels: usize, out: &mut [f32]) -> usize {
    let frames = (interleaved.len() / channels.max(1)).min(out.len());
    for (f, o) in out.iter_mut().take(frames).enumerate() {
        let s = &interleaved[f * channels..(f + 1) * channels];
        *o = s.iter().sum::<f32>() / channels as f32;
    }
    frames
}

fn device_name(d: &cpal::Device) -> String {
    d.description()
        .map(|x| x.name().to_string())
        .unwrap_or_else(|_| "Unknown device".into())
}

fn pick(
    devices: impl Iterator<Item = cpal::Device>,
    default: Option<cpal::Device>,
    sel: &DeviceSelector,
) -> Result<cpal::Device, DeviceError> {
    let all: Vec<cpal::Device> = devices
        .filter(|d| !is_roommesh_device(&device_name(d)))
        .collect();
    match sel {
        DeviceSelector::Name(n) => all
            .into_iter()
            .find(|d| &device_name(d) == n)
            .ok_or(DeviceError::NotFound),
        DeviceSelector::Default => match default {
            Some(d) if !is_roommesh_device(&device_name(&d)) => Ok(d),
            _ => all.into_iter().next().ok_or(DeviceError::NotFound),
        },
    }
}

pub fn list_devices() -> Vec<DeviceInfo> {
    let host = cpal::default_host();
    let di = host.default_input_device().map(|d| device_name(&d));
    let dout = host.default_output_device().map(|d| device_name(&d));
    let mut v = vec![];
    if let Ok(it) = host.input_devices() {
        for d in it {
            let n = device_name(&d);
            if !is_roommesh_device(&n) {
                v.push(DeviceInfo {
                    is_default: di.as_ref() == Some(&n),
                    name: n,
                    is_input: true,
                    is_output: false,
                });
            }
        }
    }
    if let Ok(it) = host.output_devices() {
        for d in it {
            let n = device_name(&d);
            if !is_roommesh_device(&n) {
                v.push(DeviceInfo {
                    is_default: dout.as_ref() == Some(&n),
                    name: n,
                    is_input: false,
                    is_output: true,
                });
            }
        }
    }
    v
}

fn build_capture(sel: &DeviceSelector) -> Result<(cpal::Stream, CaptureHandle), DeviceError> {
    let host = cpal::default_host();
    let device = pick(
        host.input_devices().map_err(be)?,
        host.default_input_device(),
        sel,
    )?;
    let name = device_name(&device);
    let config: cpal::StreamConfig = device.default_input_config().map_err(be)?.config();
    let channels = config.channels as usize;
    let rate = config.sample_rate;
    let (mut prod, cons) = rtrb::RingBuffer::<CaptureBlock>::new(128);
    let mut counter = 0u64;
    let stream = device
        .build_input_stream::<f32, _, _>(
            config,
            move |data: &[f32], info: &cpal::InputCallbackInfo| {
                let ts = info.timestamp();
                // cpal 0.18: `StreamInstant::duration_since` takes the earlier instant by value and
                // returns `Duration` directly (saturating to zero), not `Option<Duration>`.
                let lag = ts.callback.duration_since(ts.capture).as_nanos() as u64;
                let mut capture_ns = now_ns().saturating_sub(lag);
                let frames = data.len() / channels;
                let mut done = 0;
                while done < frames {
                    let n = (frames - done).min(BLOCK_FRAMES);
                    let mut b = CaptureBlock {
                        first_frame: counter,
                        capture_ns,
                        len: n as u16,
                        samples: [0.0; BLOCK_FRAMES],
                    };
                    downmix_into(
                        &data[done * channels..(done + n) * channels],
                        channels,
                        &mut b.samples[..n],
                    );
                    let _ = prod.push(b); // full → dropped; counter still advances so the timeline zero-fills
                    counter += n as u64;
                    capture_ns += n as u64 * 1_000_000_000 / rate as u64;
                    done += n;
                }
            },
            |e| log::error!("capture stream error: {e}"),
            None,
        )
        .map_err(be)?;
    stream.play().map_err(be)?;
    Ok((
        stream,
        CaptureHandle {
            blocks: cons,
            sample_rate: rate,
            device_name: name,
        },
    ))
}

fn build_playback(sel: &DeviceSelector) -> Result<(cpal::Stream, PlaybackHandle), DeviceError> {
    let host = cpal::default_host();
    let device = pick(
        host.output_devices().map_err(be)?,
        host.default_output_device(),
        sel,
    )?;
    let name = device_name(&device);
    let config: cpal::StreamConfig = device.default_output_config().map_err(be)?.config();
    let channels = config.channels as usize;
    let rate = config.sample_rate;
    let (prod, mut cons) = rtrb::RingBuffer::<f32>::new(rate as usize);
    let (mut rep_prod, rep_cons) = rtrb::RingBuffer::<PlaybackReport>::new(512);
    let (mut out_frames, mut popped) = (0u64, 0u64);
    let stream = device
        .build_output_stream::<f32, _, _>(
            config,
            move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
                let ts = info.timestamp();
                let lead = ts.playback.duration_since(ts.callback).as_nanos() as u64;
                let _ = rep_prod.push(PlaybackReport {
                    output_frames: out_frames,
                    popped_frames: popped,
                    play_ns: now_ns() + lead,
                });
                for frame in data.chunks_mut(channels) {
                    let v = match cons.pop() {
                        Ok(v) => {
                            popped += 1;
                            v
                        }
                        Err(_) => 0.0,
                    };
                    frame.fill(v);
                    out_frames += 1;
                }
            },
            |e| log::error!("playback stream error: {e}"),
            None,
        )
        .map_err(be)?;
    stream.play().map_err(be)?;
    Ok((
        stream,
        PlaybackHandle {
            samples: prod,
            reports: rep_cons,
            sample_rate: rate,
            device_name: name,
        },
    ))
}

/// Maps a playback ring position to its presentation time using the callback reports.
pub struct PlaybackClock {
    clock: DeviceClock,
    underrun_offset: u64,
    last_popped: u64,
}

impl PlaybackClock {
    pub fn new(rate: f64) -> Self {
        Self {
            clock: DeviceClock::new(rate, 0),
            underrun_offset: 0,
            last_popped: 0,
        }
    }
    pub fn report(&mut self, r: PlaybackReport) {
        self.clock.report(r.output_frames, r.play_ns);
        self.underrun_offset = r.output_frames - r.popped_frames;
        self.last_popped = r.popped_frames;
    }
    pub fn popped(&self) -> u64 {
        self.last_popped
    }
    /// Presentation time of the sample that will be pushed at ring position `pos`.
    pub fn play_time_of(&self, pos: u64) -> Option<u64> {
        self.clock.time_of(pos + self.underrun_offset)
    }
}

enum Cmd {
    StartCapture(DeviceSelector, Sender<Result<CaptureHandle, DeviceError>>),
    StopCapture,
    StartPlayback(DeviceSelector, Sender<Result<PlaybackHandle, DeviceError>>),
    StopPlayback,
}

/// Owns cpal streams on a dedicated thread.
pub struct DeviceHost {
    tx: Sender<Cmd>,
}

// `cap`/`play` are read only via their Drop impl (stopping the previous stream before replacing
// or clearing it), which the unused-assignment lint can't see.
#[allow(unused_assignments)]
fn device_thread_loop(rx: crossbeam_channel::Receiver<Cmd>) {
    let (mut cap, mut play): (Option<cpal::Stream>, Option<cpal::Stream>) = (None, None);
    for cmd in rx {
        match cmd {
            Cmd::StartCapture(sel, reply) => {
                cap = None;
                let _ = reply.send(build_capture(&sel).map(|(s, h)| {
                    cap = Some(s);
                    h
                }));
            }
            Cmd::StopCapture => cap = None,
            Cmd::StartPlayback(sel, reply) => {
                play = None;
                let _ = reply.send(build_playback(&sel).map(|(s, h)| {
                    play = Some(s);
                    h
                }));
            }
            Cmd::StopPlayback => play = None,
        }
    }
}

impl DeviceHost {
    pub fn spawn() -> Self {
        let (tx, rx) = unbounded::<Cmd>();
        std::thread::Builder::new()
            .name("roommesh-devices".into())
            .spawn(move || device_thread_loop(rx))
            .expect("spawn device thread");
        Self { tx }
    }
    pub fn start_capture(&self, sel: DeviceSelector) -> Result<CaptureHandle, DeviceError> {
        let (tx, rx) = bounded(1);
        self.tx.send(Cmd::StartCapture(sel, tx)).map_err(be)?;
        rx.recv().map_err(be)?
    }
    pub fn stop_capture(&self) {
        let _ = self.tx.send(Cmd::StopCapture);
    }
    pub fn start_playback(&self, sel: DeviceSelector) -> Result<PlaybackHandle, DeviceError> {
        let (tx, rx) = bounded(1);
        self.tx.send(Cmd::StartPlayback(sel, tx)).map_err(be)?;
        rx.recv().map_err(be)?
    }
    pub fn stop_playback(&self) {
        let _ = self.tx.send(Cmd::StopPlayback);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn downmix_averages_channels() {
        let mut out = [0.0f32; 2];
        assert_eq!(downmix_into(&[1.0, 0.0, 0.5, 0.5], 2, &mut out), 2);
        assert_eq!(out, [0.5, 0.5]);
        assert_eq!(downmix_into(&[0.3, 0.1], 1, &mut out), 2);
        assert_eq!(out, [0.3, 0.1]);
    }
    #[test]
    fn virtual_devices_are_never_physical() {
        assert!(is_roommesh_device("RoomMesh Microphone"));
        assert!(!is_roommesh_device("MacBook Pro Microphone"));
    }
    #[test]
    fn playback_queue_position_maps_to_output_time() {
        let mut pq = PlaybackClock::new(48_000.0);
        for i in 0..20u64 {
            pq.report(PlaybackReport {
                output_frames: i * 512,
                popped_frames: i * 512 - (i.min(3) * 10),
                play_ns: 1_000_000_000 + i * 512 * 1_000_000_000 / 48_000,
            });
        }
        // ring position 20*512 (+30 underrun frames) will be output at counter 20*512+30
        let t = pq.play_time_of(20 * 512).unwrap() as f64;
        let want = 1.0e9 + (20.0 * 512.0 + 30.0) * 1e9 / 48_000.0;
        assert!((t - want).abs() < 100_000.0, "{t} {want}");
    }
    #[test]
    #[ignore = "needs real audio hardware"]
    fn capture_default_device_for_one_second() {
        let host = DeviceHost::spawn();
        let mut cap = host.start_capture(DeviceSelector::Default).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
        let mut frames = 0;
        while let Ok(b) = cap.blocks.pop() {
            frames += b.len as usize;
        }
        assert!(frames > cap.sample_rate as usize / 2);
    }
}
