//! Transport abstraction. The room and audio layers only see this trait; Apple P2P
//! (Swift/Network.framework, via FFI) is the first implementation, LAN can be added later.
use crate::ids::PeerId;

#[derive(Clone, Debug, PartialEq)]
pub struct LocalAdvertisement {
    pub peer_id: PeerId,
    pub name: String,
    pub protocol_version: u16,
}

/// Implementations must be idempotent: `connect` to an already connected/connecting peer is a
/// no-op, and simultaneous dials must collapse to one connection (keep the one dialed by the
/// lower PeerId).
pub trait PeerTransport: Send + Sync {
    /// Advertise the local peer and browse for others (Bonjour `_roomaudio._tcp`/`_udp`).
    fn start(&self, advert: LocalAdvertisement);
    fn stop(&self);
    fn connect(&self, peer: PeerId);
    fn disconnect(&self, peer: PeerId);
    /// Reliable, ordered, framed.
    fn send_control(&self, peer: PeerId, frame: Vec<u8>);
    /// Unreliable datagram; may be dropped or reordered.
    fn send_realtime(&self, peer: PeerId, packet: Vec<u8>);
    /// Human readable transport/interface currently in use (e.g. "awdl0", "en0", "loopback").
    fn description(&self) -> String {
        "unknown".into()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum TransportEvent {
    Discovered { peer: PeerId, name: String },
    Lost(PeerId),
    Connected(PeerId),
    Disconnected(PeerId),
    Control { peer: PeerId, frame: Vec<u8> },
    Realtime(Vec<u8>),
}

/// Should be an unbounded sender (or otherwise never block on a full/rendezvous channel):
/// implementations such as `LoopbackNetwork` may hold an internal lock shared by every peer
/// while resolving which sinks to notify, so a sink whose `send` blocks (or is slow) can stall
/// unrelated peers even when events are queued for sending only after the lock is released.
pub type TransportSink = crossbeam_channel::Sender<TransportEvent>;
