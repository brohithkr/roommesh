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

    func testRecoveryBackoffDoublesToCapAndResets() {
        var b = RecoveryBackoff()
        XCTAssertEqual((0..<8).map { _ in b.nextDelay() }, [1.5, 3, 6, 12, 24, 30, 30, 30])
        b.reset()
        XCTAssertEqual(b.nextDelay(), 1.5)
        var many = RecoveryBackoff()
        for _ in 0..<10_000 { _ = many.nextDelay() }
        XCTAssertEqual(many.nextDelay(), 30, "no overflow after many failures")
    }

    func testDeniedLatchFiresOnlyOnTransition() {
        var latch = DeniedLatch()
        XCTAssertTrue(latch.markDenied(), "first denial reports")
        XCTAssertFalse(latch.markDenied(), "repeated denial while retrying is silent")
        XCTAssertFalse(latch.markDenied())
        latch.markAllowed()
        XCTAssertTrue(latch.markDenied(), "denied again after being allowed reports again")
    }

    func testVanishedPeers() {
        XCTAssertEqual(AppleP2PTransport.vanishedPeers(previous: ["a", "b", "c"], current: ["b", "d"]), ["a", "c"])
        XCTAssertEqual(AppleP2PTransport.vanishedPeers(previous: [], current: ["b"]), [])
        XCTAssertEqual(AppleP2PTransport.vanishedPeers(previous: ["a"], current: []), ["a"])
    }

    func testPeerNameFiltersEndpoints() {
        let svc = { (name: String) in NWEndpoint.service(name: name, type: AppleP2PTransport.controlType, domain: "local.", interface: nil) }
        XCTAssertEqual(AppleP2PTransport.peerName(svc(high), localId: low), high)
        XCTAssertNil(AppleP2PTransport.peerName(svc(low), localId: low), "self")
        XCTAssertNil(AppleP2PTransport.peerName(svc("short"), localId: low), "not a peer id")
        XCTAssertNil(AppleP2PTransport.peerName(.hostPort(host: "127.0.0.1", port: 1), localId: low))
    }
}
