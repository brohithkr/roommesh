//! App-side client of the RoomMesh driver's shared memory: writes the arbitrated room mic into
//! the mic ring (served by "RoomMesh Microphone") and reads what meeting apps play into
//! "RoomMesh Speaker" from the speaker ring.
//!
//! Sample slots are accessed as `AtomicU32` (f32 bits, `Relaxed`); ordering between the sample
//! data and the ring positions comes from the `Release` store / `Acquire` load of `write_pos`.
//!
//! Trust: the driver (running inside coreaudiod as `_coreaudiod`) creates the region with mode
//! 0666, so any local process can map it, read the room audio and write into either ring. We
//! only open a region owned by `_coreaudiod` (so another user can't create the name first and
//! feed us their own region), and treat every sample read from it as untrusted: non-finite
//! samples become silence and the rest are clamped to [-1, 1].
use crate::audio::shared_layout::*;
use std::ffi::CString;
use std::ptr::addr_of_mut;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum VirtualDeviceError {
    #[error("RoomMesh driver shared memory not found ({0}) — is the driver installed and coreaudiod restarted?")]
    NotFound(i32),
    #[error("shared memory has unexpected size or version")]
    Incompatible,
    #[error("RoomMesh driver shared memory is not initialised yet")]
    NotReady,
    #[error("system call failed: {0}")]
    Os(i32),
    #[error("RoomMesh driver shared memory is owned by uid {0}, not _coreaudiod; refusing it")]
    UntrustedOwner(u32),
}

/// The uid of the `_coreaudiod` user the driver runs as (202 on current macOS).
pub fn coreaudiod_uid() -> u32 {
    const FALLBACK: u32 = 202;
    // SAFETY: getpwnam returns null or a pointer to a static passwd entry, read immediately.
    // Only called when (re)opening the region, never from a realtime thread.
    unsafe {
        let pw = libc::getpwnam(c"_coreaudiod".as_ptr());
        if pw.is_null() {
            FALLBACK
        } else {
            (*pw).pw_uid
        }
    }
}

/// Owners we accept for the driver's region: `_coreaudiod`, plus (in this crate's unit tests
/// only) ourselves, so regions from `create_for_test` can be reopened.
fn trusted_owners() -> Vec<u32> {
    #[allow(unused_mut)]
    let mut v = vec![coreaudiod_uid()];
    #[cfg(test)]
    v.push(unsafe { libc::geteuid() });
    v
}

/// Replaces NaN/Inf with silence and clamps to [-1, 1].
fn scrub(v: f32) -> f32 {
    if v.is_finite() {
        v.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

pub struct SharedRegion {
    ptr: *mut SharedLayout,
    name: String,
    owner: bool,
    generation: u64,
}
unsafe impl Send for SharedRegion {}
unsafe impl Sync for SharedRegion {}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(-1)
}

/// The sample slot `pos` of `ring` as an atomic cell.
///
/// # Safety
/// `ring` must point into a live mapping of a `SharedLayout`.
unsafe fn slot<'a>(ring: *mut Ring, pos: u64) -> &'a AtomicU32 {
    // f32 and u32 have the same size and alignment; the slot is only ever accessed atomically.
    AtomicU32::from_ptr(addr_of_mut!((*ring).samples[(pos & RING_MASK) as usize]).cast())
}

/// # Safety
/// See [`slot`].
unsafe fn load_sample(ring: *mut Ring, pos: u64) -> f32 {
    f32::from_bits(slot(ring, pos).load(Ordering::Relaxed))
}

/// # Safety
/// See [`slot`].
unsafe fn store_sample(ring: *mut Ring, pos: u64, v: f32) {
    slot(ring, pos).store(v.to_bits(), Ordering::Relaxed)
}

impl SharedRegion {
    /// Opens the driver's region `name`, refusing one not owned by `_coreaudiod`.
    pub fn open(name: &str) -> Result<Self, VirtualDeviceError> {
        Self::open_owned_by(name, &trusted_owners())
    }

    /// [`open`](Self::open), accepting only a region whose owner is one of `owners`.
    pub fn open_owned_by(name: &str, owners: &[u32]) -> Result<Self, VirtualDeviceError> {
        let c = CString::new(name).map_err(|_| VirtualDeviceError::Incompatible)?;
        unsafe {
            let fd = libc::shm_open(c.as_ptr(), libc::O_RDWR, 0 as libc::c_uint);
            if fd < 0 {
                return Err(VirtualDeviceError::NotFound(errno()));
            }
            let mut st: libc::stat = std::mem::zeroed();
            if libc::fstat(fd, &mut st) != 0 {
                let e = errno();
                libc::close(fd);
                return Err(VirtualDeviceError::Os(e));
            }
            if !owners.contains(&st.st_uid) {
                libc::close(fd);
                return Err(VirtualDeviceError::UntrustedOwner(st.st_uid));
            }
            // Created but not sized yet: the driver is still setting the region up.
            if st.st_size == 0 {
                libc::close(fd);
                return Err(VirtualDeviceError::NotReady);
            }
            // macOS rounds shm sizes up to a page multiple
            if (st.st_size as usize) < std::mem::size_of::<SharedLayout>() {
                libc::close(fd);
                return Err(VirtualDeviceError::Incompatible);
            }
            let p = libc::mmap(
                std::ptr::null_mut(),
                std::mem::size_of::<SharedLayout>(),
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            let e = errno();
            libc::close(fd);
            if p == libc::MAP_FAILED {
                return Err(VirtualDeviceError::Os(e));
            }
            let mut r = Self {
                ptr: p as *mut SharedLayout,
                name: name.to_string(),
                owner: false,
                generation: 0,
            };
            // The driver fills in the static header fields and then publishes a non-zero
            // generation (Release); read it first (Acquire) so the fields below are initialised.
            let generation = r.header().generation.load(Ordering::Acquire);
            if generation == 0 {
                return Err(VirtualDeviceError::NotReady);
            }
            r.generation = generation;
            let h = r.header();
            if h.magic != SHM_MAGIC
                || h.version != SHM_VERSION
                || h.ring_frames as usize != RING_FRAMES
                || h.sample_rate != 48_000
            {
                return Err(VirtualDeviceError::Incompatible);
            }
            Ok(r)
        }
    }

    /// Creates a private region with the driver's initial state (tests and the devtool only).
    pub fn create_for_test() -> Result<Self, VirtualDeviceError> {
        Self::create_for_test_with(1, 0)
    }

    /// [`create_for_test`](Self::create_for_test) with a chosen `generation` (non-zero) and
    /// initial driver heartbeat.
    pub fn create_for_test_with(
        generation: u64,
        driver_heartbeat_ns: u64,
    ) -> Result<Self, VirtualDeviceError> {
        let mut b = [0u8; 4];
        getrandom::fill(&mut b).expect("rng");
        let name = format!("/rmtest.{:08x}", u32::from_le_bytes(b));
        let c = CString::new(name.clone()).unwrap();
        unsafe {
            let fd = libc::shm_open(
                c.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
                0o600 as libc::c_uint,
            );
            if fd < 0 {
                return Err(VirtualDeviceError::Os(errno()));
            }
            let fail = |fd: libc::c_int| {
                let e = errno();
                libc::close(fd);
                libc::shm_unlink(c.as_ptr());
                VirtualDeviceError::Os(e)
            };
            let size = std::mem::size_of::<SharedLayout>();
            if libc::ftruncate(fd, size as libc::off_t) != 0 {
                return Err(fail(fd));
            }
            let p = libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            if p == libc::MAP_FAILED {
                return Err(fail(fd));
            }
            libc::close(fd);
            std::ptr::write_bytes(p as *mut u8, 0, size);
            let lp = p as *mut SharedLayout;
            (*lp).header.magic = SHM_MAGIC;
            (*lp).header.version = SHM_VERSION;
            (*lp).header.sample_rate = 48_000;
            (*lp).header.ring_frames = RING_FRAMES as u32;
            (*lp)
                .header
                .driver_heartbeat_ns
                .store(driver_heartbeat_ns, Ordering::Relaxed);
            (*lp)
                .header
                .generation
                .store(generation.max(1), Ordering::Release);
            Ok(Self {
                ptr: lp,
                name,
                owner: true,
                generation: generation.max(1),
            })
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn header(&self) -> &SharedHeader {
        unsafe { &(*self.ptr).header }
    }
    /// The driver generation this mapping was opened at (changes when the driver restarts and
    /// recreates the region).
    pub fn generation(&self) -> u64 {
        self.generation
    }
    fn mic_ring(&self) -> (*mut Ring, &RingHeader) {
        unsafe { (addr_of_mut!((*self.ptr).mic), &(*self.ptr).mic.h) }
    }
    fn speaker_ring(&self) -> (*mut Ring, &RingHeader) {
        unsafe { (addr_of_mut!((*self.ptr).speaker), &(*self.ptr).speaker.h) }
    }

    pub fn driver_alive(&self, now_ns: u64) -> bool {
        now_ns.saturating_sub(self.header().driver_heartbeat_ns.load(Ordering::Relaxed))
            < 2_000_000_000
    }

    // ---- driver-side emulation, used by tests and `roommesh-devtool shm-selftest` ----
    pub fn test_set_driver_heartbeat(&self, ns: u64) {
        self.header()
            .driver_heartbeat_ns
            .store(ns, Ordering::Relaxed);
    }
    pub fn test_mic_write_pos(&self) -> u64 {
        self.mic_ring().1.write_pos.load(Ordering::Acquire)
    }
    pub fn test_driver_read_mic(&self, from_pos: u64, out: &mut [f32]) -> usize {
        let (ring, h) = self.mic_ring();
        let w = h.write_pos.load(Ordering::Acquire);
        let n = (w.saturating_sub(from_pos) as usize).min(out.len());
        for (i, o) in out.iter_mut().take(n).enumerate() {
            *o = unsafe { load_sample(ring, from_pos + i as u64) };
        }
        h.read_pos.store(from_pos + n as u64, Ordering::Release);
        n
    }
    pub fn test_driver_write_speaker(&self, samples: &[f32], host_ns: u64) {
        let (ring, h) = self.speaker_ring();
        let w = h.write_pos.load(Ordering::Relaxed);
        for (i, s) in samples.iter().enumerate() {
            unsafe { store_sample(ring, w + i as u64, *s) };
        }
        // Mirrors the real driver's SharedRegion::WriteSpeaker seqlock protocol (see
        // RingHeader::seq in shared_layout.rs) so tests exercise the same sequence a real
        // driver produces, rather than leaving seq at 0 (which would only ever exercise the
        // old-driver fallback path in write_state below).
        let s = h.seq.load(Ordering::Relaxed);
        h.seq.store(s + 1, Ordering::Relaxed);
        std::sync::atomic::fence(Ordering::Release);
        h.write_host_ns.store(host_ns, Ordering::Relaxed);
        h.write_pos
            .store(w + samples.len() as u64, Ordering::Release);
        h.seq.store(s + 2, Ordering::Release);
    }
    /// Emulates a driver from before the seqlock existed: publishes write_pos/write_host_ns the
    /// old way, leaving `seq` at 0. Used to test [`SpeakerReader`]'s fallback path.
    pub fn test_driver_write_speaker_legacy(&self, samples: &[f32], host_ns: u64) {
        let (ring, h) = self.speaker_ring();
        let w = h.write_pos.load(Ordering::Relaxed);
        for (i, s) in samples.iter().enumerate() {
            unsafe { store_sample(ring, w + i as u64, *s) };
        }
        h.write_host_ns.store(host_ns, Ordering::Release);
        h.write_pos
            .store(w + samples.len() as u64, Ordering::Release);
    }
}

impl Drop for SharedRegion {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(
                self.ptr as *mut libc::c_void,
                std::mem::size_of::<SharedLayout>(),
            );
            if self.owner {
                let c = CString::new(self.name.clone()).unwrap();
                libc::shm_unlink(c.as_ptr());
            }
        }
    }
}

/// Writes the room mic into the mic ring. Dropping the writer zeroes the app heartbeat so the
/// driver serves silence immediately instead of waiting for the heartbeat to go stale.
pub struct MicWriter {
    region: Arc<SharedRegion>,
}

impl MicWriter {
    pub fn new(region: Arc<SharedRegion>) -> Self {
        Self { region }
    }
    pub fn region(&self) -> &Arc<SharedRegion> {
        &self.region
    }
    pub fn write(&self, samples: &[f32], now_ns: u64) {
        let (ring, h) = self.region.mic_ring();
        let w = h.write_pos.load(Ordering::Relaxed);
        for (i, s) in samples.iter().enumerate() {
            unsafe { store_sample(ring, w + i as u64, *s) };
        }
        h.write_host_ns.store(now_ns, Ordering::Relaxed);
        h.write_pos
            .store(w + samples.len() as u64, Ordering::Release);
        self.region
            .header()
            .app_heartbeat_ns
            .store(now_ns, Ordering::Relaxed);
    }
    pub fn clients(&self) -> u32 {
        self.region.header().mic_clients.load(Ordering::Relaxed)
    }
}

impl Drop for MicWriter {
    fn drop(&mut self) {
        self.region
            .header()
            .app_heartbeat_ns
            .store(0, Ordering::Release);
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SpeakerChunk {
    pub first_pos: u64,
    pub write_pos: u64,
    pub write_host_ns: u64,
    pub samples: Vec<f32>,
}

pub struct SpeakerReader {
    region: Arc<SharedRegion>,
    cursor: Option<u64>,
}

impl SpeakerReader {
    pub fn new(region: Arc<SharedRegion>) -> Self {
        Self {
            region,
            cursor: None,
        }
    }
    pub fn region(&self) -> &Arc<SharedRegion> {
        &self.region
    }
    /// A consistent (write_pos, write_host_ns) pair, using the driver's seqlock
    /// (RingHeader::seq; see SharedRegion::WriteSpeaker in driver/src/SharedRegion.cpp):
    /// read seq, retry if odd (a write is in progress); read write_pos and write_host_ns;
    /// fence; re-read seq and retry if it changed underneath us. seq == 0 means a driver from
    /// before the seqlock existed is running (it never writes this field), so fall back to the
    /// previous heuristic instead of waiting forever on a seqlock that will never advance.
    fn write_state(h: &RingHeader) -> (u64, u64) {
        const MAX_ATTEMPTS: u32 = 8;
        for _ in 0..MAX_ATTEMPTS {
            let seq1 = h.seq.load(Ordering::Acquire);
            if seq1 == 0 {
                return Self::write_state_legacy_heuristic(h);
            }
            if seq1 & 1 != 0 {
                continue; // odd: a write is in progress right now, retry
            }
            let w = h.write_pos.load(Ordering::Relaxed);
            let t = h.write_host_ns.load(Ordering::Relaxed);
            std::sync::atomic::fence(Ordering::Acquire);
            let seq2 = h.seq.load(Ordering::Relaxed);
            if seq1 == seq2 {
                return (w, t);
            }
            // seq changed while we were reading w/t (a write started and/or completed
            // concurrently): the pair we just read may be torn, retry.
        }
        // Exhausted the bounded retries (e.g. the driver is writing unusually fast, or seq is
        // stuck odd because the driver crashed mid-write). Fall back to a plain Acquire read of
        // each field rather than spinning forever - a torn pair here is no worse than what the
        // pre-seqlock heuristic could already produce in the same pathological case.
        (
            h.write_pos.load(Ordering::Acquire),
            h.write_host_ns.load(Ordering::Acquire),
        )
    }
    /// Pre-seqlock heuristic, kept for regions written by an older driver (see write_state
    /// above): the host time is read on both sides of the position and the read retried
    /// (bounded) if the driver published in between.
    fn write_state_legacy_heuristic(h: &RingHeader) -> (u64, u64) {
        let mut state = (0, 0);
        for _ in 0..4 {
            let t1 = h.write_host_ns.load(Ordering::Acquire);
            let w = h.write_pos.load(Ordering::Acquire);
            let t2 = h.write_host_ns.load(Ordering::Acquire);
            state = (w, t2);
            if t1 == t2 {
                break;
            }
        }
        state
    }
    /// Reads up to `max` new samples. The first call only positions the cursor at the live edge.
    pub fn read(&mut self, max: usize) -> Option<SpeakerChunk> {
        let (ring, h) = self.region.speaker_ring();
        let (w, wt) = Self::write_state(h);
        let cur = self.cursor.get_or_insert(w);
        if w < *cur || w - *cur > (RING_FRAMES - 4_800) as u64 {
            *cur = w.saturating_sub(960);
        }
        let n = ((w - *cur) as usize).min(max);
        if n == 0 {
            return None;
        }
        let first = *cur;
        let samples = (0..n as u64)
            .map(|i| scrub(unsafe { load_sample(ring, first + i) }))
            .collect();
        *cur += n as u64;
        h.read_pos.store(*cur, Ordering::Release);
        Some(SpeakerChunk {
            first_pos: first,
            write_pos: w,
            write_host_ns: wt,
            samples,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mic_writer_and_speaker_reader_roundtrip_through_shm() {
        let region = Arc::new(SharedRegion::create_for_test().unwrap());
        let mic = MicWriter::new(region.clone());
        mic.write(&[0.5; 480], 1_000);
        mic.write(&[0.25; 480], 2_000);
        // driver side: read mic
        let mut got = vec![0.0f32; 960];
        assert_eq!(region.test_driver_read_mic(0, &mut got), 960);
        assert_eq!((got[0], got[959]), (0.5, 0.25));
        assert_eq!(
            region.header().app_heartbeat_ns.load(Ordering::Relaxed),
            2_000
        );
        // driver side: write speaker, app reads from the live edge
        let mut spk = SpeakerReader::new(region.clone());
        region.test_driver_write_speaker(&[0.1; 100], 5_000);
        assert!(spk.read(4096).is_none(), "reader starts at the live edge");
        region.test_driver_write_speaker(&[0.2; 300], 6_000);
        let chunk = spk.read(4096).unwrap();
        assert_eq!(
            (chunk.first_pos, chunk.samples.len(), chunk.write_host_ns),
            (100, 300, 6_000)
        );
        assert!(chunk.samples.iter().all(|v| *v == 0.2));
    }
    #[test]
    fn reader_recovers_from_overrun_and_open_validates() {
        let region = Arc::new(SharedRegion::create_for_test().unwrap());
        let mut spk = SpeakerReader::new(region.clone());
        region.test_driver_write_speaker(&[0.0; 10], 1);
        spk.read(10);
        for _ in 0..80 {
            region.test_driver_write_speaker(&[0.3; 480], 2);
        } // > ring size
        let c = spk.read(1 << 20).unwrap();
        assert!(c.samples.len() <= RING_FRAMES);
        assert!(SharedRegion::open("/roommesh.nonexistent").is_err());
        let reopened = SharedRegion::open(region.name()).unwrap();
        assert_eq!(reopened.header().magic, SHM_MAGIC);
    }
    #[test]
    fn open_records_generation_and_rejects_uninitialised_region() {
        let region = SharedRegion::create_for_test_with(7, 123).unwrap();
        assert_eq!(region.generation(), 7);
        assert!(region.driver_alive(123 + 1_000_000_000));
        assert!(!region.driver_alive(123 + 3_000_000_000));
        assert_eq!(SharedRegion::open(region.name()).unwrap().generation(), 7);
        region.header().generation.store(0, Ordering::Release);
        assert!(matches!(
            SharedRegion::open(region.name()),
            Err(VirtualDeviceError::NotReady)
        ));
        region.header().generation.store(8, Ordering::Release);
        let r = SharedRegion::open(region.name()).unwrap();
        assert_eq!(r.generation(), 8);
        r.test_set_driver_heartbeat(5);
        assert_eq!(
            region.header().driver_heartbeat_ns.load(Ordering::Relaxed),
            5
        );
    }
    #[test]
    fn open_reports_an_unsized_region_as_not_ready() {
        let mut b = [0u8; 4];
        getrandom::fill(&mut b).expect("rng");
        let name = format!("/rmtest.{:08x}", u32::from_le_bytes(b));
        let c = CString::new(name.clone()).unwrap();
        unsafe {
            let fd = libc::shm_open(
                c.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
                0o600 as libc::c_uint,
            );
            assert!(fd >= 0);
            libc::close(fd);
        }
        let r = SharedRegion::open(&name);
        unsafe {
            libc::shm_unlink(c.as_ptr());
        }
        assert!(matches!(r, Err(VirtualDeviceError::NotReady)));
    }
    #[test]
    fn dropping_mic_writer_zeroes_app_heartbeat() {
        let region = Arc::new(SharedRegion::create_for_test().unwrap());
        let w = MicWriter::new(region.clone());
        w.write(&[0.1; 480], 42);
        assert_eq!(region.header().app_heartbeat_ns.load(Ordering::Relaxed), 42);
        assert_eq!(region.test_mic_write_pos(), 480);
        drop(w);
        assert_eq!(region.header().app_heartbeat_ns.load(Ordering::Acquire), 0);
    }

    #[test]
    fn speaker_seqlock_advances_and_reader_gets_consistent_state() {
        let region = Arc::new(SharedRegion::create_for_test().unwrap());
        assert_eq!(region.speaker_ring().1.seq.load(Ordering::Relaxed), 0);
        let mut spk = SpeakerReader::new(region.clone());
        region.test_driver_write_speaker(&[0.1; 10], 1_000);
        assert_eq!(region.speaker_ring().1.seq.load(Ordering::Relaxed), 2);
        assert!(spk.read(4096).is_none(), "reader starts at the live edge");

        region.test_driver_write_speaker(&[0.2; 20], 2_000);
        assert_eq!(region.speaker_ring().1.seq.load(Ordering::Relaxed), 4);
        let chunk = spk.read(4096).unwrap();
        assert_eq!(chunk.write_pos, 30);
        assert_eq!(chunk.write_host_ns, 2_000);
        assert_eq!(chunk.samples.len(), 20);
        assert!(chunk.samples.iter().all(|v| *v == 0.2));
    }

    #[test]
    fn speaker_reader_falls_back_to_heuristic_for_pre_seqlock_driver() {
        let region = Arc::new(SharedRegion::create_for_test().unwrap());
        let mut spk = SpeakerReader::new(region.clone());
        // Legacy write path never touches seq - it stays 0, the reader's "old driver" signal
        // to fall back to the write_host_ns-matching heuristic instead of a seqlock retry loop
        // that would otherwise spin until it exhausts MAX_ATTEMPTS.
        region.test_driver_write_speaker_legacy(&[0.1; 5], 1_000);
        assert_eq!(region.speaker_ring().1.seq.load(Ordering::Relaxed), 0);
        assert!(spk.read(4096).is_none(), "reader starts at the live edge");

        region.test_driver_write_speaker_legacy(&[0.3; 15], 5_000);
        assert_eq!(region.speaker_ring().1.seq.load(Ordering::Relaxed), 0);
        let chunk = spk.read(4096).unwrap();
        assert_eq!(chunk.write_pos, 20);
        assert_eq!(chunk.write_host_ns, 5_000);
        assert_eq!(chunk.samples.len(), 15);
        assert!(chunk.samples.iter().all(|v| *v == 0.3));
    }

    #[test]
    fn open_rejects_a_region_not_owned_by_coreaudiod() {
        // A region any local user could have created first ("squatting" the driver's name):
        // only the `_coreaudiod` user's region is trusted outside of tests.
        let region = SharedRegion::create_for_test().unwrap();
        let me = unsafe { libc::geteuid() };
        assert_ne!(me, coreaudiod_uid(), "tests don't run as _coreaudiod");
        assert!(matches!(
            SharedRegion::open_owned_by(region.name(), &[coreaudiod_uid()]),
            Err(VirtualDeviceError::UntrustedOwner(uid)) if uid == me
        ));
        assert!(SharedRegion::open_owned_by(region.name(), &[coreaudiod_uid(), me]).is_ok());
        // `open` itself trusts our own uid only in this crate's unit tests.
        assert!(SharedRegion::open(region.name()).is_ok());
    }
    #[test]
    fn coreaudiod_uid_is_looked_up_by_name() {
        let pw = unsafe { libc::getpwnam(c"_coreaudiod".as_ptr()) };
        let expected = if pw.is_null() {
            202
        } else {
            unsafe { (*pw).pw_uid }
        };
        assert_eq!(coreaudiod_uid(), expected);
    }
    #[test]
    fn speaker_reader_scrubs_non_finite_and_out_of_range_samples() {
        // The ring is writable by any local process (the driver creates it 0666), so what we
        // read is untrusted input to the DSP and codec.
        let region = Arc::new(SharedRegion::create_for_test().unwrap());
        let mut spk = SpeakerReader::new(region.clone());
        region.test_driver_write_speaker(&[0.0; 4], 1);
        assert!(spk.read(4096).is_none());
        region.test_driver_write_speaker(
            &[
                f32::NAN,
                f32::INFINITY,
                f32::NEG_INFINITY,
                3.0,
                -7.5,
                0.25,
                -1.0,
            ],
            2,
        );
        let c = spk.read(4096).unwrap();
        assert_eq!(c.samples, vec![0.0, 0.0, 0.0, 1.0, -1.0, 0.25, -1.0]);
    }

    #[test]
    #[ignore = "requires the RoomMesh HAL driver installed and coreaudiod running"]
    fn opens_live_driver_region() {
        // Strictly `_coreaudiod`-owned (plain `open` also trusts our own uid in unit tests).
        let region = SharedRegion::open_owned_by(SHM_NAME, &[coreaudiod_uid()])
            .expect("driver shared memory not found or not owned by _coreaudiod");
        let h = region.header();
        assert_eq!(h.magic, SHM_MAGIC);
        assert_eq!(h.version, SHM_VERSION);
        assert_eq!(h.ring_frames as usize, RING_FRAMES);
        assert!(
            region.driver_alive(crate::time::now_ns()),
            "driver heartbeat is stale"
        );
    }
}
