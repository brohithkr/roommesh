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
    /// Inbound links start with a tiny limit; it is raised once the preamble identifies the dialer.
    var decoder: FrameDecoder
    var announced = false
    init(conn: NWConnection, outbound: Bool, peerId: String?) {
        self.conn = conn; self.outbound = outbound; self.peerId = peerId
        decoder = FrameDecoder(maxFrame: peerId == nil ? Preamble.maxFrame : Framing.maxFrame)
    }
}

/// `FfiTransport` over Network.framework with Apple peer-to-peer (AWDL) enabled.
/// Control: Bonjour `_roomaudio._tcp`, length-prefixed frames. Realtime: `_roomaudio._udp` datagrams.
/// All mutable state is confined to `queue`; `description()` is lock-protected because Rust calls it from its own threads.
/// The core authenticates control frames and realtime packets, so this layer only has to stay robust.
final class AppleP2PTransport: FfiTransport, @unchecked Sendable {
    static let controlType = "_roomaudio._tcp"
    static let realtimeType = "_roomaudio._udp"

    /// Unidentified inbound connections that may wait for a preamble at once; more are refused.
    static let maxPending = 32
    /// An inbound control connection must send its preamble within this many seconds.
    static let preambleTimeout: TimeInterval = 5
    /// An outbound dial that has not reached `.ready` by then is abandoned (the core re-requests).
    static let dialTimeout: TimeInterval = 8
    /// Delay before recreating a failed listener or browser.
    static let restartBackoff: TimeInterval = 1.5
    /// Inbound realtime connections with no datagram for this long are cancelled.
    static let realtimeIdleTimeout: UInt64 = 30 * NSEC_PER_SEC
    static let realtimeSweepInterval: TimeInterval = 5

    weak var sink: TransportSink?
    var onLocalNetworkDenied: (@Sendable () -> Void)?
    /// Fired when the browser reaches `.ready`, i.e. local network access works.
    var onLocalNetworkAllowed: (@Sendable () -> Void)?

    private let queue = DispatchQueue(label: "io.github.brohithkr.RoomMesh.transport", qos: .userInteractive)
    private var running = false
    private var localId = ""
    private var txt = NWTXTRecord()
    private var controlListener: NWListener?
    private var realtimeListener: NWListener?
    private var browser: NWBrowser?
    private var links: [String: ControlLink] = [:]
    private var pending: [ObjectIdentifier: ControlLink] = [:]
    private var udp: [String: NWConnection] = [:]
    private var inboundUDP: [ObjectIdentifier: NWConnection] = [:]
    private var inboundLastRx: [ObjectIdentifier: UInt64] = [:]
    private var sweepTimer: DispatchSourceTimer?
    private let descLock = NSLock()
    private var interfaceDescription = "Apple peer-to-peer"

    static func tcpParameters() -> NWParameters {
        let tcp = NWProtocolTCP.Options()
        tcp.noDelay = true
        tcp.enableKeepalive = true
        tcp.keepaliveIdle = 2
        tcp.connectionTimeout = 5
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

    // MARK: pure decisions (unit-tested)

    /// Tie-break between a new control link and an existing one for the same peer: keep the
    /// connection dialed by the lower peer id; if both were dialed by the same side, keep the newest.
    /// Both peers evaluate this with swapped roles and agree on the survivor.
    static func preferNew(localId: String, peerId: String, newOutbound: Bool, existingOutbound: Bool) -> Bool {
        let newDialer = newOutbound ? localId : peerId
        let oldDialer = existingOutbound ? localId : peerId
        return newDialer <= oldDialer
    }

    /// Outbound realtime states after which the cached connection must be discarded and re-dialed.
    static func udpStateIsTerminal(_ state: NWConnection.State) -> Bool {
        switch state {
        case .failed, .waiting, .cancelled: return true
        default: return false
        }
    }

    /// Keys whose last receive time is more than `timeout` before `now`.
    static func idleKeys<K: Hashable>(_ lastRx: [K: UInt64], now: UInt64, timeout: UInt64) -> [K] {
        lastRx.compactMap { key, t in now > t && now - t > timeout ? key : nil }
    }

    static func admitsInbound(pendingCount: Int) -> Bool { pendingCount < maxPending }

    // MARK: FfiTransport
    func start(peerId: String, name: String, protocolVersion: UInt16) {
        queue.async { [self] in
            running = true
            localId = peerId
            txt = NWTXTRecord(["n": name, "v": String(protocolVersion)])
            startListeners()
            startBrowser()
            startSweepTimer()
        }
    }

    func stop() {
        queue.async { [self] in
            running = false
            sweepTimer?.cancel(); sweepTimer = nil
            browser?.cancel(); controlListener?.cancel(); realtimeListener?.cancel()
            browser = nil; controlListener = nil; realtimeListener = nil
            links.values.forEach { $0.conn.cancel() }
            pending.values.forEach { $0.conn.cancel() }
            udp.values.forEach { $0.cancel() }
            inboundUDP.values.forEach { $0.cancel() }
            links = [:]; pending = [:]; udp = [:]; inboundUDP = [:]; inboundLastRx = [:]
        }
    }

    func connect(peerId: String) {
        queue.async { [self] in
            guard running, peerId != localId, links[peerId] == nil,
                  !pending.values.contains(where: { $0.outbound && $0.peerId == peerId }) else { return }
            let conn = NWConnection(to: .service(name: peerId, type: Self.controlType, domain: "local.", interface: nil),
                                    using: Self.tcpParameters())
            let link = ControlLink(conn: conn, outbound: true, peerId: peerId)
            track(link)
            expireIfPending(link, after: Self.dialTimeout)
        }
    }

    func disconnect(peerId: String) {
        queue.async { [self] in
            links[peerId]?.conn.cancel()
            evictRealtime(peerId)
        }
    }

    func sendControl(peerId: String, frame: Data) {
        queue.async { [self] in
            links[peerId]?.conn.send(content: Framing.encode(frame), completion: .contentProcessed { _ in })
        }
    }

    func sendRealtime(peerId: String, packet: Data) {
        queue.async { [self] in
            guard running else { return }
            let conn: NWConnection
            if let c = udp[peerId] {
                conn = c
            } else {
                conn = NWConnection(to: .service(name: peerId, type: Self.realtimeType, domain: "local.", interface: nil),
                                    using: Self.udpParameters())
                conn.stateUpdateHandler = { [weak self, weak conn] s in
                    guard Self.udpStateIsTerminal(s), let self, let conn else { return }
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

    // MARK: listeners and browser
    private func startListeners() {
        startControlListener()
        startRealtimeListener()
    }

    private func startControlListener() {
        guard running else { return }
        controlListener?.cancel(); controlListener = nil
        do {
            let l = try NWListener(using: Self.tcpParameters())
            l.service = NWListener.Service(name: localId, type: Self.controlType, domain: nil, txtRecord: txt)
            l.newConnectionHandler = { [weak self] c in self?.accept(c) }
            l.stateUpdateHandler = { [weak self, weak l] s in
                guard let self, let l else { return }
                switch s {
                case .waiting(let e): self.checkDenied(e)
                case .failed(let e):
                    self.checkDenied(e)
                    NSLog("RoomMesh: control listener failed: \(e)")
                    l.cancel()
                    guard self.controlListener === l else { return }
                    self.controlListener = nil
                    self.restartLater { $0.controlListener == nil ? $0.startControlListener() : () }
                default: break
                }
            }
            l.start(queue: queue)
            controlListener = l
        } catch {
            NSLog("RoomMesh: control listener failed: \(error)")
            restartLater { $0.controlListener == nil ? $0.startControlListener() : () }
        }
    }

    private func startRealtimeListener() {
        guard running else { return }
        realtimeListener?.cancel(); realtimeListener = nil
        do {
            let l = try NWListener(using: Self.udpParameters())
            l.service = NWListener.Service(name: localId, type: Self.realtimeType, domain: nil, txtRecord: txt)
            l.newConnectionHandler = { [weak self] c in self?.acceptRealtime(c) }
            l.stateUpdateHandler = { [weak self, weak l] s in
                guard let self, let l else { return }
                switch s {
                case .waiting(let e): self.checkDenied(e)
                case .failed(let e):
                    self.checkDenied(e)
                    NSLog("RoomMesh: realtime listener failed: \(e)")
                    l.cancel()
                    guard self.realtimeListener === l else { return }
                    self.realtimeListener = nil
                    self.restartLater { $0.realtimeListener == nil ? $0.startRealtimeListener() : () }
                default: break
                }
            }
            l.start(queue: queue)
            realtimeListener = l
        } catch {
            NSLog("RoomMesh: realtime listener failed: \(error)")
            restartLater { $0.realtimeListener == nil ? $0.startRealtimeListener() : () }
        }
    }

    private func startBrowser() {
        guard running else { return }
        browser?.cancel(); browser = nil
        let b = NWBrowser(for: .bonjourWithTXTRecord(type: Self.controlType, domain: nil), using: Self.tcpParameters())
        b.browseResultsChangedHandler = { [weak self] _, changes in self?.browse(changes) }
        b.stateUpdateHandler = { [weak self, weak b] s in
            guard let self, let b else { return }
            switch s {
            case .ready: self.onLocalNetworkAllowed?()
            case .waiting(let e): self.checkDenied(e)
            case .failed(let e):
                self.checkDenied(e)
                NSLog("RoomMesh: browser failed: \(e)")
                b.cancel()
                guard self.browser === b else { return }
                self.browser = nil
                self.restartLater { $0.browser == nil ? $0.startBrowser() : () }
            default: break
            }
        }
        b.start(queue: queue)
        browser = b
    }

    /// Runs `body` after the restart backoff unless the transport has been stopped meanwhile.
    private func restartLater(_ body: @escaping (AppleP2PTransport) -> Void) {
        queue.asyncAfter(deadline: .now() + Self.restartBackoff) { [weak self] in
            guard let self, self.running else { return }
            body(self)
        }
    }

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
            evictRealtime(name) // a restarted peer gets a fresh realtime connection
            sink?.peerLost(name)
        }
    }
    private func checkDenied(_ e: NWError) {
        if case let .dns(code) = e, code == -65570 { onLocalNetworkDenied?() } // kDNSServiceErr_PolicyDenied
    }

    // MARK: control links
    private func accept(_ conn: NWConnection) {
        guard running, Self.admitsInbound(pendingCount: pending.count) else { conn.cancel(); return }
        let link = ControlLink(conn: conn, outbound: false, peerId: nil)
        track(link)
        expireIfPending(link, after: Self.preambleTimeout)
    }

    /// Cancels `link` if it is still pending (unidentified inbound, or a dial that never got ready).
    private func expireIfPending(_ link: ControlLink, after delay: TimeInterval) {
        queue.asyncAfter(deadline: .now() + delay) { [weak self, weak link] in
            guard let self, let link, self.pending[ObjectIdentifier(link)] === link else { return }
            link.conn.cancel()
        }
    }

    private func track(_ link: ControlLink) {
        pending[ObjectIdentifier(link)] = link
        link.conn.stateUpdateHandler = { [weak self, weak link] s in
            guard let self, let link else { return }
            switch s {
            case .ready:
                if link.outbound {
                    link.conn.send(content: Framing.encode(Preamble.make(peerId: self.localId)), completion: .contentProcessed { _ in })
                    guard self.promote(link) else { return } // lost the tie-break; already cancelled
                }
                self.updateInterface(link.conn)
                self.receive(link)
            case .waiting(let e):
                self.checkDenied(e)
                link.conn.cancel() // the core re-requests connections periodically
            case .failed:
                link.conn.cancel()
                self.drop(link)
            case .cancelled:
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
                link.decoder.append(data)
                do { while let f = try link.decoder.next() { self.handle(f, on: link) } }
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
            link.decoder.maxFrame = Framing.maxFrame // buffered bytes are kept
            promote(link)
            return
        }
        guard link.announced, let id = link.peerId, links[id] === link else { return }
        sink?.controlFrame(id, frame)
    }

    /// Makes `link` the announced control link for its peer. Duplicates collapse via `preferNew`.
    /// A replaced announced link is reported as disconnect + connect so the Rust handshake restarts.
    /// Returns false (and cancels `link`) when the existing link wins.
    @discardableResult
    private func promote(_ link: ControlLink) -> Bool {
        guard let id = link.peerId else { return false }
        pending[ObjectIdentifier(link)] = nil
        if let existing = links[id], existing !== link {
            guard Self.preferNew(localId: localId, peerId: id, newOutbound: link.outbound, existingOutbound: existing.outbound) else {
                link.conn.cancel()
                return false
            }
            links[id] = nil
            existing.conn.cancel()
            evictRealtime(id)
            if existing.announced { sink?.disconnected(id) }
        }
        links[id] = link
        link.announced = true
        sink?.connected(id)
        return true
    }

    /// Idempotent: safe to call for both `.failed` and the `.cancelled` that follows.
    private func drop(_ link: ControlLink) {
        pending[ObjectIdentifier(link)] = nil
        guard let id = link.peerId, links[id] === link else { return }
        links[id] = nil
        if link.announced {
            evictRealtime(id)
            sink?.disconnected(id)
        }
    }

    private func updateInterface(_ conn: NWConnection) {
        guard let i = conn.currentPath?.availableInterfaces.first else { return }
        let kind = i.name.hasPrefix("awdl") ? "Apple peer-to-peer" : (i.type == .wifi ? "Wi-Fi" : (i.type == .wiredEthernet ? "Ethernet" : "network"))
        descLock.lock(); interfaceDescription = "\(kind) (\(i.name))"; descLock.unlock()
    }

    // MARK: realtime
    /// A connected UDP socket stays `.ready` after its remote vanishes, so the cache is evicted
    /// whenever the peer's control link goes away or its service disappears.
    private func evictRealtime(_ id: String) {
        udp.removeValue(forKey: id)?.cancel()
    }

    private func acceptRealtime(_ conn: NWConnection) {
        guard running else { conn.cancel(); return }
        let key = ObjectIdentifier(conn)
        inboundUDP[key] = conn
        inboundLastRx[key] = DispatchTime.now().uptimeNanoseconds
        conn.stateUpdateHandler = { [weak self, weak conn] s in
            guard let self, let conn else { return }
            switch s {
            case .failed, .cancelled: self.closeInbound(conn)
            default: break
            }
        }
        conn.start(queue: queue)
        receiveDatagrams(conn)
    }

    private func receiveDatagrams(_ conn: NWConnection) {
        conn.receiveMessage { [weak self, weak conn] data, _, _, error in
            guard let self, let conn, self.inboundUDP[ObjectIdentifier(conn)] === conn else { return }
            if let data, !data.isEmpty {
                self.inboundLastRx[ObjectIdentifier(conn)] = DispatchTime.now().uptimeNanoseconds
                self.sink?.realtimePacket(data)
            }
            if error == nil { self.receiveDatagrams(conn) } else { self.closeInbound(conn) }
        }
    }

    private func closeInbound(_ conn: NWConnection) {
        let key = ObjectIdentifier(conn)
        guard inboundUDP[key] === conn else { return }
        inboundUDP[key] = nil
        inboundLastRx[key] = nil
        conn.cancel()
    }

    private func startSweepTimer() {
        sweepTimer?.cancel()
        let t = DispatchSource.makeTimerSource(queue: queue)
        t.schedule(deadline: .now() + Self.realtimeSweepInterval, repeating: Self.realtimeSweepInterval)
        t.setEventHandler { [weak self] in self?.sweepIdleRealtime() }
        t.resume()
        sweepTimer = t
    }

    private func sweepIdleRealtime() {
        let idle = Self.idleKeys(inboundLastRx, now: DispatchTime.now().uptimeNanoseconds, timeout: Self.realtimeIdleTimeout)
        for key in idle { if let conn = inboundUDP[key] { closeInbound(conn) } }
    }
}
