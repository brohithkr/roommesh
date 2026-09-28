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
    /// Orders deliveries (see `with_lock_and_flush`): taken *before* `inner` and held through
    /// both computing a batch of events under `inner` and sending it, so batches are delivered
    /// in the order their state changes were made. `inner` itself is released before the sends
    /// (a sink may block -- see `TransportSink`'s doc); without `emit`, two threads could
    /// deliver their batches in the opposite order to the one they computed them in.
    emit: Mutex<()>,
}

impl LoopbackNetwork {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner::default()),
            emit: Mutex::new(()),
        })
    }

    pub fn transport(self: &Arc<Self>, me: PeerId, sink: TransportSink) -> Arc<LoopbackTransport> {
        self.inner.lock().nodes.insert(
            me,
            Node {
                name: String::new(),
                sink,
                advertised: false,
            },
        );
        Arc::new(LoopbackTransport {
            net: self.clone(),
            me,
        })
    }

    /// Looks up a node's sink (a cheap clone of the channel sender) without sending anything.
    /// Callers collect the (sink, event) pairs they need to emit under `inner`'s lock, from
    /// within the closure passed to `with_lock_and_flush`.
    fn sink(inner: &Inner, to: PeerId) -> Option<TransportSink> {
        inner.nodes.get(&to).map(|n| n.sink.clone())
    }

    /// Acquires `emit` *before* `inner`, runs `f` with `inner` locked to compute the batch of
    /// (sink, event) pairs to deliver, releases `inner`, then sends the batch while still
    /// holding `emit`. Acquiring `emit` first (and holding it through the sends) means the order
    /// in which threads acquire `emit` -- and thus the order their events are delivered in --
    /// mirrors the order in which they acquired `inner`, instead of whichever thread happens to
    /// reach the send loop first. `inner` itself is still only held for the brief state
    /// read/mutate step, never across the (potentially blocking) sends.
    fn with_lock_and_flush(
        &self,
        f: impl FnOnce(&mut Inner) -> Vec<(TransportSink, TransportEvent)>,
    ) {
        let _order = self.emit.lock();
        let sends = {
            let mut g = self.inner.lock();
            f(&mut g)
        };
        for (s, ev) in sends {
            let _ = s.send(ev);
        }
    }

    pub fn partition(&self, a: PeerId, b: PeerId) {
        self.with_lock_and_flush(|g| {
            g.blocked.insert(key(a, b));
            let mut sends = Vec::new();
            if g.links.remove(&key(a, b)) {
                if let Some(s) = Self::sink(g, a) {
                    sends.push((s, TransportEvent::Disconnected(b)));
                }
                if let Some(s) = Self::sink(g, b) {
                    sends.push((s, TransportEvent::Disconnected(a)));
                }
            }
            sends
        });
    }
    pub fn heal(&self, a: PeerId, b: PeerId) {
        self.inner.lock().blocked.remove(&key(a, b));
    }

    /// Simulates a crash: drops all links of `peer` and removes it.
    pub fn remove(&self, peer: PeerId) {
        self.with_lock_and_flush(|g| {
            let others: Vec<PeerId> = g
                .links
                .iter()
                .filter(|(x, y)| *x == peer || *y == peer)
                .map(|(x, y)| if *x == peer { *y } else { *x })
                .collect();
            let mut sends = Vec::new();
            for o in others {
                g.links.remove(&key(peer, o));
                if let Some(s) = Self::sink(g, o) {
                    sends.push((s.clone(), TransportEvent::Disconnected(peer)));
                    sends.push((s, TransportEvent::Lost(peer)));
                }
            }
            g.nodes.remove(&peer);
            sends
        });
    }
}

pub struct LoopbackTransport {
    net: Arc<LoopbackNetwork>,
    me: PeerId,
}

impl PeerTransport for LoopbackTransport {
    fn start(&self, advert: LocalAdvertisement) {
        self.net.with_lock_and_flush(|g| {
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
            let mut sends = Vec::new();
            for (id, name) in others {
                if let Some(s) = LoopbackNetwork::sink(g, self.me) {
                    sends.push((s, TransportEvent::Discovered { peer: id, name }));
                }
                if let Some(s) = LoopbackNetwork::sink(g, id) {
                    sends.push((
                        s,
                        TransportEvent::Discovered {
                            peer: self.me,
                            name: advert.name.clone(),
                        },
                    ));
                }
            }
            sends
        });
    }
    fn stop(&self) {
        let mut g = self.net.inner.lock();
        if let Some(n) = g.nodes.get_mut(&self.me) {
            n.advertised = false;
        }
    }
    fn connect(&self, peer: PeerId) {
        self.net.with_lock_and_flush(|g| {
            let k = key(self.me, peer);
            if g.blocked.contains(&k) || g.links.contains(&k) || !g.nodes.contains_key(&peer) {
                return Vec::new();
            }
            g.links.insert(k);
            let mut sends = Vec::new();
            if let Some(s) = LoopbackNetwork::sink(g, self.me) {
                sends.push((s, TransportEvent::Connected(peer)));
            }
            if let Some(s) = LoopbackNetwork::sink(g, peer) {
                sends.push((s, TransportEvent::Connected(self.me)));
            }
            sends
        });
    }
    fn disconnect(&self, peer: PeerId) {
        self.net.with_lock_and_flush(|g| {
            let mut sends = Vec::new();
            if g.links.remove(&key(self.me, peer)) {
                if let Some(s) = LoopbackNetwork::sink(g, self.me) {
                    sends.push((s, TransportEvent::Disconnected(peer)));
                }
                if let Some(s) = LoopbackNetwork::sink(g, peer) {
                    sends.push((s, TransportEvent::Disconnected(self.me)));
                }
            }
            sends
        });
    }
    fn send_control(&self, peer: PeerId, frame: Vec<u8>) {
        self.net.with_lock_and_flush(|g| {
            if !g.links.contains(&key(self.me, peer)) {
                return Vec::new();
            }
            match LoopbackNetwork::sink(g, peer) {
                Some(s) => vec![(
                    s,
                    TransportEvent::Control {
                        peer: self.me,
                        frame,
                    },
                )],
                None => Vec::new(),
            }
        });
    }
    fn send_realtime(&self, peer: PeerId, packet: Vec<u8>) {
        // A link is not required (realtime packets may legitimately arrive before `connect`
        // completes on a lossy/unreliable transport); only the partition check applies here.
        // `sink()` already returns `None` when the peer node doesn't exist (e.g. it was removed
        // via `LoopbackNetwork::remove`, simulating a crash), so no separate existence check is
        // needed.
        self.net.with_lock_and_flush(|g| {
            if g.blocked.contains(&key(self.me, peer)) {
                return Vec::new();
            }
            match LoopbackNetwork::sink(g, peer) {
                Some(s) => vec![(s, TransportEvent::Realtime(packet))],
                None => Vec::new(),
            }
        });
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
        ta.start(LocalAdvertisement {
            peer_id: PeerId(1),
            name: "A".into(),
            protocol_version: 1,
        });
        tb.start(LocalAdvertisement {
            peer_id: PeerId(2),
            name: "B".into(),
            protocol_version: 1,
        });
        assert!(ra.try_iter().any(|e| e
            == TransportEvent::Discovered {
                peer: PeerId(2),
                name: "B".into()
            }));
        rb.try_iter().count();
        ta.connect(PeerId(2));
        assert!(rb
            .try_iter()
            .any(|e| e == TransportEvent::Connected(PeerId(1))));
        ta.send_control(PeerId(2), vec![1]);
        ta.send_realtime(PeerId(2), vec![2]);
        let got: Vec<_> = rb.try_iter().collect();
        assert!(got.contains(&TransportEvent::Control {
            peer: PeerId(1),
            frame: vec![1]
        }));
        assert!(got.contains(&TransportEvent::Realtime(vec![2])));
        net.partition(PeerId(1), PeerId(2));
        assert!(rb
            .try_iter()
            .any(|e| e == TransportEvent::Disconnected(PeerId(1))));
        ta.send_realtime(PeerId(2), vec![3]);
        assert_eq!(rb.try_iter().count(), 0);
    }
}
