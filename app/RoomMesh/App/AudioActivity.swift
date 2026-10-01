import Foundation

/// Keeps App Nap away while this Mac carries room audio (in a room, or while Settings' noise
/// meter runs). RoomMesh is a menu-bar app without a visible window, which is exactly what App
/// Nap throttles: timers get coalesced and threads deprioritised, so the audio thread would
/// wake late and the room speaker would gap. An activity with `.latencyCritical` turns that off.
///
/// `update(carryingAudio:)` is idempotent: the activity is taken once and ended once.
@MainActor
final class AudioActivity {
    typealias Begin = () -> NSObjectProtocol
    typealias End = (NSObjectProtocol) -> Void

    nonisolated static let reason = "RoomMesh is carrying room audio"

    private let begin: Begin
    private let end: End
    private var token: NSObjectProtocol?

    /// `begin`/`end`: the test seam (default: `ProcessInfo`'s activity API).
    init(begin: @escaping Begin = {
             ProcessInfo.processInfo.beginActivity(options: [.userInitiated, .latencyCritical], reason: AudioActivity.reason)
         },
         end: @escaping End = { ProcessInfo.processInfo.endActivity($0) }) {
        self.begin = begin
        self.end = end
    }

    /// Whether the activity is held.
    var isHeld: Bool { token != nil }

    func update(carryingAudio: Bool) {
        if carryingAudio {
            if token == nil { token = begin() }
        } else if let token {
            self.token = nil
            end(token)
        }
    }
}
