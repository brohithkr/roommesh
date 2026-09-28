import Network
import XCTest
@testable import RoomMesh

/// Pure decision logic extracted from `AppleP2PTransport`.
final class TransportLogicTests: XCTestCase {
    private let low = "0000000000000001"
    private let high = "fffffffffffffff0"

    // Tie-break: keep the connection dialed by the lower peer id; same dialer keeps the newest.
    func testPreferNewWhenLocalIsLower() {
        // new outbound (dialer = local, lower) vs existing inbound (dialer = peer, higher) → keep new
        XCTAssertTrue(AppleP2PTransport.preferNew(localId: low, peerId: high, newOutbound: true, existingOutbound: false))
        // new inbound (dialer = peer, higher) vs existing outbound (dialer = local, lower) → keep existing
        XCTAssertFalse(AppleP2PTransport.preferNew(localId: low, peerId: high, newOutbound: false, existingOutbound: true))
    }
    func testPreferNewWhenLocalIsHigher() {
        XCTAssertFalse(AppleP2PTransport.preferNew(localId: high, peerId: low, newOutbound: true, existingOutbound: false))
        XCTAssertTrue(AppleP2PTransport.preferNew(localId: high, peerId: low, newOutbound: false, existingOutbound: true))
    }
    func testSameDialerKeepsNewest() {
        for (local, peer) in [(low, high), (high, low)] {
            XCTAssertTrue(AppleP2PTransport.preferNew(localId: local, peerId: peer, newOutbound: true, existingOutbound: true))
            XCTAssertTrue(AppleP2PTransport.preferNew(localId: local, peerId: peer, newOutbound: false, existingOutbound: false))
        }
    }
    func testTieBreakIsSymmetricAcrossPeers() {
        // Both sides must agree on which of the two cross-dialed connections survives.
        // A's outbound is B's inbound. A keeps its outbound iff B keeps its inbound.
        let aKeepsOutbound = AppleP2PTransport.preferNew(localId: low, peerId: high, newOutbound: true, existingOutbound: false)
        let bKeepsInbound = AppleP2PTransport.preferNew(localId: high, peerId: low, newOutbound: false, existingOutbound: true)
        XCTAssertEqual(aKeepsOutbound, bKeepsInbound)
    }

    func testUDPWaitingAndFailedAreTerminal() {
        XCTAssertTrue(AppleP2PTransport.udpStateIsTerminal(.failed(.posix(.ECONNREFUSED))))
        XCTAssertTrue(AppleP2PTransport.udpStateIsTerminal(.waiting(.posix(.ENETDOWN))))
        XCTAssertTrue(AppleP2PTransport.udpStateIsTerminal(.cancelled))
        XCTAssertFalse(AppleP2PTransport.udpStateIsTerminal(.ready))
        XCTAssertFalse(AppleP2PTransport.udpStateIsTerminal(.preparing))
        XCTAssertFalse(AppleP2PTransport.udpStateIsTerminal(.setup))
    }

    func testIdleKeys() {
        let s: UInt64 = 1_000_000_000
        let lastRx = ["fresh": 95 * s, "edge": 70 * s, "stale": 60 * s]
        XCTAssertEqual(Set(AppleP2PTransport.idleKeys(lastRx, now: 100 * s, timeout: 30 * s)), ["stale"])
        XCTAssertTrue(AppleP2PTransport.idleKeys([String: UInt64](), now: 100 * s, timeout: 30 * s).isEmpty)
        // A clock reading older than lastRx must not underflow.
        XCTAssertTrue(AppleP2PTransport.idleKeys(["x": 100 * s], now: 50 * s, timeout: 30 * s).isEmpty)
    }

    func testPendingCap() {
        XCTAssertTrue(AppleP2PTransport.admitsInbound(pendingCount: 0))
        XCTAssertTrue(AppleP2PTransport.admitsInbound(pendingCount: 31))
        XCTAssertFalse(AppleP2PTransport.admitsInbound(pendingCount: 32))
        XCTAssertFalse(AppleP2PTransport.admitsInbound(pendingCount: 100))
    }
}
