import Foundation
@testable import RoomMesh

final class FakeCore: RoomMeshCoreProtocol, @unchecked Sendable {
    var calls: [String] = []
    var room: FfiRoomState?
    var fail: Error?
    func start() {}
    func stop() {}
    func localPeerId() -> String { "000000000000000a" }
    func onPeerDiscovered(peerId: String, name: String) {}
    func onPeerLost(peerId: String) {}
    func onConnected(peerId: String) {}
    func onDisconnected(peerId: String) {}
    func onControlFrame(peerId: String, frame: Data) {}
    func onRealtimePacket(packet: Data) {}
    private func record(_ s: String) throws { calls.append(s); if let fail { throw fail } }
    func createRoom(name: String) throws { try record("create:\(name)") }
    func invite(peerId: String) throws { try record("invite:\(peerId)") }
    func respondToInvite(roomId: String, accept: Bool) throws { try record("respond:\(roomId):\(accept)") }
    func leaveRoom() throws { try record("leave") }
    func setCoordinator(peerId: String) throws { try record("coord:\(peerId)") }
    func setSpeaker(peerId: String?) throws { try record("speaker:\(peerId ?? "nil")") }
    func setPeerMicrophoneEnabled(peerId: String, enabled: Bool) throws { try record("mic:\(peerId):\(enabled)") }
    func removeMember(peerId: String) throws { try record("remove:\(peerId)") }
    func renameRoom(name: String) throws { try record("rename:\(name)") }
    func setLocalMute(muted: Bool) { calls.append("mute:\(muted)") }
    func isLocalMuted() -> Bool { false }
    func startAudioEngine() {}
    func stopAudioEngine() {}
    var lastSettings: FfiSettings?
    func updateSettings(settings: FfiSettings) { calls.append("settings"); lastSettings = settings }
    var meter: FfiMicMeter?
    func setMicMeterEnabled(enabled: Bool) { calls.append("meter:\(enabled)") }
    func getMicMeter() -> FfiMicMeter? { meter }
    func setLocalInfo(name: String, driverInstalled: Bool) {}
    func getRoomState() -> FfiRoomState? { room }
    func getNearbyPeers() -> [FfiNearbyPeer] { [] }
    var metrics: [FfiPeerMetrics] = []
    func getPeerMetrics() -> [FfiPeerMetrics] { metrics }
    func getActiveMicrophones() -> [String] { [] }
    func virtualDeviceAvailable() -> Bool { true }
}

func member(_ id: String, _ name: String, local: Bool = false, coord: Bool = false, speaker: Bool = false, active: Bool = false, online: Bool = true,
            driver: Bool = true) -> FfiMember {
    FfiMember(id: id, name: name, isLocal: local, online: online, micEnabled: true, isCoordinator: coord, isSpeaker: speaker, isActiveMic: active,
              driverInstalled: driver)
}
func sampleRoom() -> FfiRoomState {
    FfiRoomState(roomId: "00000000000000ff", name: "Conference Room", epoch: 1, coordinator: "000000000000000a",
                 speaker: "000000000000000c", activePrimary: "000000000000000b", activeSecondary: nil,
                 members: [member("000000000000000a", "Rohith's MacBook", local: true, coord: true),
                           member("000000000000000b", "Amaan's MacBook", active: true),
                           member("000000000000000c", "Meeting MacBook", speaker: true),
                           member("000000000000000d", "Puyan's MacBook", online: false)])
}
