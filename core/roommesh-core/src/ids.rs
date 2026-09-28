//! Strongly typed identifiers shared by room, network and audio layers.
use serde::{Deserialize, Serialize};
use std::fmt;

fn random_u64() -> u64 {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).expect("system RNG unavailable");
    u64::from_le_bytes(b)
}

macro_rules! hex_id {
    ($name:ident) => {
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub struct $name(pub u64);
        impl $name {
            pub fn random() -> Self {
                Self(random_u64())
            }
            pub fn to_hex(&self) -> String {
                format!("{:016x}", self.0)
            }
            pub fn from_hex(s: &str) -> Option<Self> {
                // Only accept the exact canonical lowercase form `to_hex()` produces: uppercase
                // hex or stray characters (e.g. a leading '+' that `u64::from_str_radix` would
                // otherwise happily accept as a sign) are rejected.
                if s.len() != 16 || !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
                    return None;
                }
                u64::from_str_radix(s, 16).ok().map(Self)
            }
        }
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), &self.to_hex()[..6])
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.to_hex())
            }
        }
    };
}
hex_id!(PeerId);
hex_id!(RoomId);

/// Coordinator term. Strictly increases on every coordinator change.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default, Serialize, Deserialize,
)]
pub struct Epoch(pub u32);
impl Epoch {
    pub fn next(self) -> Epoch {
        Epoch(self.0.saturating_add(1))
    }
}

/// Identifies one realtime stream from one sender.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct StreamId(pub u32);
impl StreamId {
    pub const MIC: StreamId = StreamId(1);
    pub const FAR_END: StreamId = StreamId(2);
    pub const CLOCK: StreamId = StreamId(3);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn peer_id_hex_roundtrip() {
        let p = PeerId(0x00ab_cdef_0123_4567);
        assert_eq!(p.to_hex(), "00abcdef01234567");
        assert_eq!(PeerId::from_hex("00abcdef01234567"), Some(p));
        assert_eq!(PeerId::from_hex("xyz"), None);
    }
    #[test]
    fn from_hex_rejects_non_canonical_input() {
        assert_eq!(PeerId::from_hex("00ABCDEF01234567"), None); // uppercase
        assert_eq!(PeerId::from_hex("+0abcdef01234567"), None); // sign char accepted by from_str_radix
                                                                // Every value's to_hex() output must still round-trip.
        for v in [0u64, 1, u64::MAX, 0x00ab_cdef_0123_4567] {
            let p = PeerId(v);
            assert_eq!(PeerId::from_hex(&p.to_hex()), Some(p));
        }
    }
    #[test]
    fn random_ids_differ() {
        assert_ne!(PeerId::random(), PeerId::random());
        assert_ne!(RoomId::random(), RoomId::random());
    }
    #[test]
    fn epoch_orders_and_increments() {
        assert!(Epoch(41) < Epoch(42));
        assert_eq!(Epoch(41).next(), Epoch(42));
        assert_eq!(Epoch(u32::MAX).next(), Epoch(u32::MAX)); // saturates, never wraps
    }
}
