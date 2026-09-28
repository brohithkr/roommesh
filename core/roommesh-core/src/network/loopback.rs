//! In-process transport for tests: a hub routing events between peers, with partitions.
use crate::ids::PeerId;
use crate::network::transport::*;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

struct Node {
    name: String,
    sink: TransportSink,
    advertised: bool,
}

#[derive(Default)]
struct Inner {
    nodes: HashMap<PeerId, Node>,
    links: HashSet<(PeerId, PeerId)>,
    blocked: HashSet<(PeerId, PeerId)>,
}

fn key(a: PeerId, b: PeerId) -> (PeerId, PeerId) {
    if a < b {
        (a, b)
    } else {
        (b, a)
    }
}

pub struct LoopbackNetwork {
    inner: Mutex<Inner>,
}

impl LoopbackNetwork {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { inner: Mutex::new(Inner::default()) })
    }

    pub fn transport(self: &Arc<Self>, me: PeerId, sink: TransportSink) -> Arc<LoopbackTransport> {
        self.inner.lock().nodes.insert(me, Node { name: String::new(), sink, advertised: false });
        Arc::new(LoopbackTransport { net: self.clone(), me })
    }

    fn emit(inner: &Inner, to: PeerId, ev: TransportEvent) {
        if let Some(n) = inner.nodes.get(&to) {
            let _ = n.sink.send(ev);
        }
    }

    pub fn partition(&self, a: PeerId, b: PeerId) {
        let mut g = self.inner.lock();
        g.blocked.insert(key(a, b));
        if g.links.remove(&key(a, b)) {
            Self::emit(&g, a, TransportEvent::Disconnected(b));
            Self::emit(&g, b, TransportEvent::Disconnected(a));
        }
    }
    pub fn heal(&self, a: PeerId, b: PeerId) {
        self.inner.lock().blocked.remove(&key(a, b));
    }

    /// Simulates a crash: drops all links of `peer` and removes it.
    pub fn remove(&self, peer: PeerId) {
        let mut g = self.inner.lock();
        let others: Vec<PeerId> = g
            .links
            .iter()
            .filter(|(x, y)| *x == peer || *y == peer)
            .map(|(x, y)| if *x == peer { *y } else { *x })
            .collect();
        for o in others {
            g.links.remove(&key(peer, o));
            Self::emit(&g, o, TransportEvent::Disconnected(peer));
            Self::emit(&g, o, TransportEvent::Lost(peer));
        }
        g.nodes.remove(&peer);
    }
}

pub struct LoopbackTransport {
    net: Arc<LoopbackNetwork>,
    me: PeerId,
}

impl PeerTransport for LoopbackTransport {
    fn start(&self, advert: LocalAdvertisement) {
        let mut g = self.net.inner.lock();
        if let Some(n) = g.nodes.get_mut(&self.me) {
            n.name = advert.name.clone();
            n.advertised = true;
        }
        let others: Vec<(PeerId, String)> = g
            .nodes
            .iter()
            .filter(|(id, n)| **id != self.me && n.advertised)
            .map(|(id, n)| (*id, n.name.clone()))
            .collect();
        for (id, name) in others {
            LoopbackNetwork::emit(&g, self.me, TransportEvent::Discovered { peer: id, name });
            LoopbackNetwork::emit(&g, id, TransportEvent::Discovered { peer: self.me, name: advert.name.clone() });
        }
    }
    fn stop(&self) {
        let mut g = self.net.inner.lock();
        if let Some(n) = g.nodes.get_mut(&self.me) {
            n.advertised = false;
        }
    }
    fn connect(&self, peer: PeerId) {
        let mut g = self.net.inner.lock();
        let k = key(self.me, peer);
        if g.blocked.contains(&k) || g.links.contains(&k) || !g.nodes.contains_key(&peer) {
            return;
        }
        g.links.insert(k);
        LoopbackNetwork::emit(&g, self.me, TransportEvent::Connected(peer));
        LoopbackNetwork::emit(&g, peer, TransportEvent::Connected(self.me));
    }
    fn disconnect(&self, peer: PeerId) {
        let mut g = self.net.inner.lock();
        if g.links.remove(&key(self.me, peer)) {
            LoopbackNetwork::emit(&g, self.me, TransportEvent::Disconnected(peer));
            LoopbackNetwork::emit(&g, peer, TransportEvent::Disconnected(self.me));
        }
    }
    fn send_control(&self, peer: PeerId, frame: Vec<u8>) {
        let g = self.net.inner.lock();
        if g.links.contains(&key(self.me, peer)) {
            LoopbackNetwork::emit(&g, peer, TransportEvent::Control { peer: self.me, frame });
        }
    }
    fn send_realtime(&self, peer: PeerId, packet: Vec<u8>) {
        let g = self.net.inner.lock();
        if !g.blocked.contains(&key(self.me, peer)) {
            LoopbackNetwork::emit(&g, peer, TransportEvent::Realtime(packet));
        }
    }
    fn description(&self) -> String {
        "loopback".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::unbounded;
    #[test]
    fn discovery_connect_send_partition() {
        let net = LoopbackNetwork::new();
        let (sa, ra) = unbounded();
        let (sb, rb) = unbounded();
        let ta = net.transport(PeerId(1), sa);
        let tb = net.transport(PeerId(2), sb);
        ta.start(LocalAdvertisement { peer_id: PeerId(1), name: "A".into(), protocol_version: 1 });
        tb.start(LocalAdvertisement { peer_id: PeerId(2), name: "B".into(), protocol_version: 1 });
        assert!(ra.try_iter().any(|e| e == TransportEvent::Discovered { peer: PeerId(2), name: "B".into() }));
        rb.try_iter().count();
        ta.connect(PeerId(2));
        assert!(rb.try_iter().any(|e| e == TransportEvent::Connected(PeerId(1))));
        ta.send_control(PeerId(2), vec![1]);
        ta.send_realtime(PeerId(2), vec![2]);
        let got: Vec<_> = rb.try_iter().collect();
        assert!(got.contains(&TransportEvent::Control { peer: PeerId(1), frame: vec![1] }));
        assert!(got.contains(&TransportEvent::Realtime(vec![2])));
        net.partition(PeerId(1), PeerId(2));
        assert!(rb.try_iter().any(|e| e == TransportEvent::Disconnected(PeerId(1))));
        ta.send_realtime(PeerId(2), vec![3]);
        assert_eq!(rb.try_iter().count(), 0);
    }
}
