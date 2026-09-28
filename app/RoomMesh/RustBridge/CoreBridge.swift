import Foundation

/// Transport → core forwarding (the transport holds this weakly; AppModel owns it).
final class CoreSink: TransportSink, @unchecked Sendable {
    private let core: any RoomMeshCoreProtocol
    init(core: any RoomMeshCoreProtocol) { self.core = core }
    func peerDiscovered(_ peerId: String, name: String) { core.onPeerDiscovered(peerId: peerId, name: name) }
    func peerLost(_ peerId: String) { core.onPeerLost(peerId: peerId) }
    func connected(_ peerId: String) { core.onConnected(peerId: peerId) }
    func disconnected(_ peerId: String) { core.onDisconnected(peerId: peerId) }
    func controlFrame(_ peerId: String, _ frame: Data) { core.onControlFrame(peerId: peerId, frame: frame) }
    func realtimePacket(_ packet: Data) { core.onRealtimePacket(packet: packet) }
}

/// Core → UI events, delivered on Rust threads. Hops to the main queue, which is FIFO,
/// so events are applied in the order the core emitted them (separate `Task`s are not).
final class EventRelay: FfiEventListener, @unchecked Sendable {
    private weak var model: AppModel?
    init(model: AppModel) { self.model = model }
    func onEvent(event: FfiEvent) {
        DispatchQueue.main.async { [weak self] in
            MainActor.assumeIsolated { self?.model?.apply(event) }
        }
    }
}

extension FfiError {
    /// The plain message carried by every (flat) UniFFI error case.
    var message: String {
        switch self {
        case .AlreadyInRoom(let m), .NotInRoom(let m), .NoSuchInvite(let m), .NotMember(let m), .InvalidPeerId(let m),
             .Timeout(let m):
            return m
        }
    }
}

/// Human-readable message for errors thrown across the FFI.
func describe(_ error: Error) -> String {
    (error as? FfiError)?.message ?? (error as NSError).localizedDescription
}
