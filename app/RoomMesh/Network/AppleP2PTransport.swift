import Foundation
import Network

/// Callbacks into the core. Implemented by `CoreSink`.
protocol TransportSink: AnyObject, Sendable {
    func peerDiscovered(_ peerId: String, name: String)
    func peerLost(_ peerId: String)
    func connected(_ peerId: String)
    func disconnected(_ peerId: String)
    func controlFrame(_ peerId: String, _ frame: Data)
    func realtimePacket(_ packet: Data)
}

/// One TCP control connection. Only touched on the transport queue.
private final class ControlLink: @unchecked Sendable {
    let conn: NWConnection
    let outbound: Bool
    var peerId: String?
    var decoder = FrameDecoder()
    var announced = false
    init(conn: NWConnection, outbound: Bool, peerId: String?) { self.conn = conn; self.outbound = outbound; self.peerId = peerId }
}

/// `FfiTransport` over Network.framework with Apple peer-to-peer (AWDL) enabled.
/// Control: Bonjour `_roomaudio._tcp`, length-prefixed frames. Realtime: `_roomaudio._udp` datagrams.
/// All mutable state is confined to `queue`; `description()` is lock-protected because Rust calls it from its own threads.
final class AppleP2PTransport: FfiTransport, @unchecked Sendable {
    static let controlType = "_roomaudio._tcp"
    static let realtimeType = "_roomaudio._udp"

    weak var sink: TransportSink?
    var onLocalNetworkDenied: (@Sendable () -> Void)?

    private let queue = DispatchQueue(label: "io.github.brohithkr.RoomMesh.transport", qos: .userInteractive)
    private var localId = ""
    private var controlListener: NWListener?
    private var realtimeListener: NWListener?
    private var browser: NWBrowser?
    private var links: [String: ControlLink] = [:]
    private var pending: [ObjectIdentifier: ControlLink] = [:]
    private var udp: [String: NWConnection] = [:]
    private let descLock = NSLock()
    private var interfaceDescription = "Apple peer-to-peer"

    static func tcpParameters() -> NWParameters {
        let tcp = NWProtocolTCP.Options()
        tcp.noDelay = true
        tcp.enableKeepalive = true
        tcp.keepaliveIdle = 2
        let p = NWParameters(tls: nil, tcp: tcp)
        p.includePeerToPeer = true
        return p
    }
    static func udpParameters() -> NWParameters {
        let p = NWParameters.udp
        p.includePeerToPeer = true
        p.serviceClass = .interactiveVoice
        return p
    }

    // MARK: FfiTransport
    func start(peerId: String, name: String, protocolVersion: UInt16) {
        queue.async { [self] in
            localId = peerId
            let txt = NWTXTRecord(["n": name, "v": String(protocolVersion)])
            do {
                let cl = try NWListener(using: Self.tcpParameters())
                cl.service = NWListener.Service(name: peerId, type: Self.controlType, domain: nil, txtRecord: txt)
                cl.newConnectionHandler = { [weak self] c in self?.accept(c) }
                cl.stateUpdateHandler = { [weak self] s in if case .failed(let e) = s { self?.checkDenied(e) } }
                cl.start(queue: queue)
                controlListener = cl
                let rl = try NWListener(using: Self.udpParameters())
                rl.service = NWListener.Service(name: peerId, type: Self.realtimeType, domain: nil, txtRecord: txt)
                rl.newConnectionHandler = { [weak self] c in self?.acceptRealtime(c) }
                rl.start(queue: queue)
                realtimeListener = rl
            } catch {
                NSLog("RoomMesh: listener failed: \(error)")
            }
            let b = NWBrowser(for: .bonjourWithTXTRecord(type: Self.controlType, domain: nil), using: Self.tcpParameters())
            b.browseResultsChangedHandler = { [weak self] _, changes in self?.browse(changes) }
            b.stateUpdateHandler = { [weak self] s in
                switch s { case .waiting(let e), .failed(let e): self?.checkDenied(e); default: break }
            }
            b.start(queue: queue)
            browser = b
        }
    }

    func stop() {
        queue.async { [self] in
            browser?.cancel(); controlListener?.cancel(); realtimeListener?.cancel()
            browser = nil; controlListener = nil; realtimeListener = nil
            links.values.forEach { $0.conn.cancel() }
            pending.values.forEach { $0.conn.cancel() }
            udp.values.forEach { $0.cancel() }
            links = [:]; pending = [:]; udp = [:]
        }
    }

    func connect(peerId: String) {
        queue.async { [self] in
            guard peerId != localId, links[peerId] == nil,
                  !pending.values.contains(where: { $0.outbound && $0.peerId == peerId }) else { return }
            let conn = NWConnection(to: .service(name: peerId, type: Self.controlType, domain: "local.", interface: nil),
                                    using: Self.tcpParameters())
            let link = ControlLink(conn: conn, outbound: true, peerId: peerId)
            track(link)
        }
    }

    func disconnect(peerId: String) { queue.async { [self] in links[peerId]?.conn.cancel() } }

    func sendControl(peerId: String, frame: Data) {
        queue.async { [self] in
            links[peerId]?.conn.send(content: Framing.encode(frame), completion: .contentProcessed { _ in })
        }
    }

    func sendRealtime(peerId: String, packet: Data) {
        queue.async { [self] in
            let conn: NWConnection
            if let c = udp[peerId] {
                conn = c
            } else {
                conn = NWConnection(to: .service(name: peerId, type: Self.realtimeType, domain: "local.", interface: nil),
                                    using: Self.udpParameters())
                conn.stateUpdateHandler = { [weak self, weak conn] s in
                    guard case .failed = s, let self, let conn else { return }
                    conn.cancel()
                    if self.udp[peerId] === conn { self.udp[peerId] = nil } // next send re-dials
                }
                conn.start(queue: queue)
                udp[peerId] = conn
            }
            conn.send(content: packet, completion: .idempotent)
        }
    }

    func description() -> String { descLock.lock(); defer { descLock.unlock() }; return interfaceDescription }

    // MARK: discovery
    private func browse(_ changes: Set<NWBrowser.Result.Change>) {
        for change in changes {
            switch change {
            case .added(let r): report(r, present: true)
            case .changed(_, let r, _): report(r, present: true)
            case .removed(let r): report(r, present: false)
            default: break
            }
        }
    }
    private func report(_ r: NWBrowser.Result, present: Bool) {
        guard case let .service(name, _, _, _) = r.endpoint, name != localId, name.count == 16 else { return }
        if present {
            var display = name
            if case let .bonjour(txt) = r.metadata, let n = txt["n"] { display = n }
            sink?.peerDiscovered(name, name: display)
        } else {
            sink?.peerLost(name)
        }
    }
    private func checkDenied(_ e: NWError) {
        if case let .dns(code) = e, code == -65570 { onLocalNetworkDenied?() } // kDNSServiceErr_PolicyDenied
    }

    // MARK: control links
    private func accept(_ conn: NWConnection) { track(ControlLink(conn: conn, outbound: false, peerId: nil)) }

    private func track(_ link: ControlLink) {
        pending[ObjectIdentifier(link)] = link
        link.conn.stateUpdateHandler = { [weak self, weak link] s in
            guard let self, let link else { return }
            switch s {
            case .ready:
                if link.outbound {
                    link.conn.send(content: Framing.encode(Preamble.make(peerId: self.localId)), completion: .contentProcessed { _ in })
                    self.promote(link)
                }
                self.updateInterface(link.conn)
                self.receive(link)
            case .waiting(let e):
                self.checkDenied(e)
                link.conn.cancel() // the core re-requests connections periodically
            case .failed, .cancelled:
                self.drop(link)
            default: break
            }
        }
        link.conn.start(queue: queue)
    }

    private func receive(_ link: ControlLink) {
        link.conn.receive(minimumIncompleteLength: 1, maximumLength: 64 * 1024) { [weak self, weak link] data, _, done, error in
            guard let self, let link else { return }
            if let data, !data.isEmpty {
                do { for f in try link.decoder.feed(data) { self.handle(f, on: link) } }
                catch { link.conn.cancel(); return }
            }
            if done || error != nil { link.conn.cancel(); return }
            self.receive(link)
        }
    }

    private func handle(_ frame: Data, on link: ControlLink) {
        if link.peerId == nil {
            guard let id = Preamble.parse(frame), id != localId else { link.conn.cancel(); return }
            link.peerId = id
            promote(link)
            return
        }
        guard link.announced, let id = link.peerId, links[id] === link else { return }
        sink?.controlFrame(id, frame)
    }

    /// Makes `link` the announced control link for its peer. Duplicates collapse deterministically:
    /// keep the connection dialed by the lower peer id (same dialer → keep the newest). A replaced
    /// announced link is reported as disconnect + connect so the Rust handshake restarts.
    private func promote(_ link: ControlLink) {
        guard let id = link.peerId else { return }
        pending[ObjectIdentifier(link)] = nil
        if let existing = links[id], existing !== link {
            let newDialer = link.outbound ? localId : id
            let oldDialer = existing.outbound ? localId : id
            let preferNew = newDialer == oldDialer || newDialer < oldDialer
            if !preferNew { link.conn.cancel(); return }
            links[id] = nil
            existing.conn.cancel()
            if existing.announced { sink?.disconnected(id) }
        }
        links[id] = link
        link.announced = true
        sink?.connected(id)
    }

    private func drop(_ link: ControlLink) {
        pending[ObjectIdentifier(link)] = nil
        guard let id = link.peerId, links[id] === link else { return }
        links[id] = nil
        if link.announced { sink?.disconnected(id) }
    }

    private func updateInterface(_ conn: NWConnection) {
        guard let i = conn.currentPath?.availableInterfaces.first else { return }
        let kind = i.name.hasPrefix("awdl") ? "Apple peer-to-peer" : (i.type == .wifi ? "Wi-Fi" : (i.type == .wiredEthernet ? "Ethernet" : "network"))
        descLock.lock(); interfaceDescription = "\(kind) (\(i.name))"; descLock.unlock()
    }

    // MARK: realtime
    private func acceptRealtime(_ conn: NWConnection) {
        conn.start(queue: queue)
        receiveDatagrams(conn)
    }
    private func receiveDatagrams(_ conn: NWConnection) {
        conn.receiveMessage { [weak self] data, _, _, error in
            if let data, !data.isEmpty { self?.sink?.realtimePacket(data) }
            if error == nil { self?.receiveDatagrams(conn) } else { conn.cancel() }
        }
    }
}
