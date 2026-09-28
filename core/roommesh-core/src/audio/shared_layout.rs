//! Byte-for-byte mirror of driver/src/SharedLayout.hpp. Accessed only through raw pointers
//! (the other side is another process), except for the atomic fields.
use std::sync::atomic::{AtomicU32, AtomicU64};

pub const SHM_NAME: &str = "/roommesh.v1";
pub const SHM_MAGIC: u32 = 0x524D_5348;
pub const SHM_VERSION: u32 = 1;
pub const RING_FRAMES: usize = 32_768;
pub const RING_MASK: u64 = RING_FRAMES as u64 - 1;

#[repr(C)]
pub struct SharedHeader {
    pub magic: u32,
    pub version: u32,
    pub sample_rate: u32,
    pub ring_frames: u32,
    pub generation: AtomicU64,
    pub driver_heartbeat_ns: AtomicU64,
    pub app_heartbeat_ns: AtomicU64,
    pub mic_clients: AtomicU32,
    pub speaker_clients: AtomicU32,
    pub _reserved: [u64; 2],
}

#[repr(C)]
pub struct RingHeader {
    pub write_pos: AtomicU64,
    pub write_host_ns: AtomicU64,
    pub read_pos: AtomicU64,
    pub read_host_ns: AtomicU64,
    pub _reserved: [u64; 4],
}

#[repr(C)]
pub struct Ring {
    pub h: RingHeader,
    pub samples: [f32; RING_FRAMES],
}

#[repr(C)]
pub struct SharedLayout {
    pub header: SharedHeader,
    pub mic: Ring,
    pub speaker: Ring,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};
    #[test]
    fn layout_matches_driver_header() {
        assert_eq!(size_of::<SharedHeader>(), 64);
        assert_eq!(size_of::<RingHeader>(), 64);
        assert_eq!(size_of::<Ring>(), 64 + 4 * RING_FRAMES);
        assert_eq!(offset_of!(SharedLayout, mic), 64);
        assert_eq!(offset_of!(SharedLayout, speaker), 64 + size_of::<Ring>());
        assert_eq!(size_of::<SharedLayout>(), 262_336);
        assert_eq!(offset_of!(SharedHeader, generation), 16);
        assert_eq!(offset_of!(SharedHeader, app_heartbeat_ns), 32);
        assert_eq!(offset_of!(SharedHeader, mic_clients), 40);
    }
}
