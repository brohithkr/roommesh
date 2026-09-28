#pragma once
#include "SharedLayout.hpp"
#include <string>

namespace roommesh {

uint64_t HostNowNs();

// Driver side of the shared memory. ReadMic/WriteSpeaker are realtime-safe (no locks, no allocation).
class SharedRegion {
public:
    ~SharedRegion();
    // Creates (replacing any stale region) the shm object with mode 0666.
    bool Create(const char* name = kShmName);
    void Destroy();  // unmap + unlink (tests)
    SharedLayout* layout() const { return layout_; }
    bool valid() const { return layout_ != nullptr; }

    // Valid after Create() returns false: the errno captured at the failing
    // syscall, and the name of that syscall/step ("shm_open", "ftruncate",
    // "mmap"), for diagnostic logging.
    int LastErrno() const { return lastErrno_; }
    const char* LastStep() const { return lastStep_; }

    // Sentinel clientId for ReadMic callers that don't track individual HAL
    // clients (tests, or any single-client caller). A dedicated value
    // rather than a plausible-looking real ID (e.g. 0) so it can never
    // collide with - and share replay-guard state with - an actual
    // aspl::Client::GetClientID().
    static constexpr uint32_t kDefaultClientId = 0xFFFFFFFEu;

    // Serve `frames` mono samples for the RoomMesh Microphone IO cycle at device sample time
    // `sampleTime`, for the client identified by `clientId` (from aspl::Client::GetClientID();
    // defaults to kDefaultClientId - a value no real aspl client ID collides with - for
    // callers, such as tests or a driver build with only one client, that don't distinguish
    // clients). Every client in the same cycle passes the same sampleTime and receives the
    // same audio.
    //
    // Reads at least max(kMicLatencyFrames, maxFramesSeen_ + kMicLatencyFrames) frames behind the
    // app's write edge (see ReadMic's `readBehind` in SharedRegion.cpp; maxFramesSeen_ is a
    // high-water mark across every client, not just this call's own `frames`, so the margin
    // never depends on which client happens to call - and resync - first). The anchor resyncs
    // only when the app has clearly (re)started, drifted far out of range in either direction
    // (reader more than ~200ms ahead of the edge, or the edge more than readBehind + 1440
    // frames ahead of the reader), or has been stuck underrunning for several consecutive
    // cycles running while the write edge is actively advancing (a writer that's merely
    // running slow, not stalled). A plain, one-off underrun (pos + frames > write edge), or a
    // sustained one against a *frozen* write edge, is instead served partially with a
    // zero-filled tail, keeping the anchor stable so it doesn't resync onto - and replay -
    // already-served audio. A per-client high-water mark (not a single shared one) guards
    // against replay so that one client's progress can never mask another's, or leak across a
    // ResetIOStats()-driven resync; a separate, session-scoped floor guards against ever
    // re-serving audio written before an app_heartbeat_ns gap, even if this exact reader never
    // personally received it. An anchor that ends up deeper than necessary (e.g. after a
    // resync computed against a since-departed larger client's margin) is trimmed forward,
    // once, after it's held a persistent margin surplus for about half a second.
    void ReadMic(double sampleTime, float* out, uint32_t frames, uint64_t nowNs, uint32_t clientId = kDefaultClientId);
    // Store the mixed RoomMesh Speaker output (interleaved `channels`) downmixed to mono.
    void WriteSpeaker(const float* interleaved, uint32_t frames, uint32_t channels, uint64_t nowNs);
    void Heartbeat(uint64_t nowNs);

    // Resets ReadMic's per-IO-session bookkeeping: the cross-client IO-cycle-size high-water
    // mark (maxFramesSeen_), the synced flag (forcing a fresh resync on the next call rather
    // than reusing a stale offset against a HAL sample-time domain that a StopIO/StartIO may
    // have just reset to 0), and the sustained-underrun/write-edge-advancing trackers
    // (underrunStreak_, underrunDeficit_, lastMicW_). Called from IOStateHandler::OnStartIO
    // when the mic device starts IO, so none of this state can linger from a previous IO
    // session (e.g. one that briefly had a larger-buffer client attached, or whose offset
    // would otherwise silently replay stale audio against the new session's sample-time
    // domain - the per-client replay guard below independently prevents that regardless, but
    // forcing a fresh resync also avoids the extra silence a stale offset would otherwise cost
    // before the guard's high-water mark naturally caught back up).
    void ResetIOStats();

    // Releases this client's per-client replay-guard slot (see
    // micClientServed_ below), if it has one. Call from
    // IOStateHandler::OnRemoveClient (the mic's ControlRequestHandler) when
    // a HAL client disconnects, so the table doesn't permanently lose a
    // slot to every client that has ever connected over coreaudiod's
    // lifetime - a long-running coreaudiod process can see far more than
    // kMaxTrackedMicClients distinct clients over time (Meet, Zoom, etc.
    // starting and stopping), and without this, the table eventually fills
    // and every *new* client's reads silently stop being guarded at all
    // (fail open). Clears servedEnd before releasing the clientId, so a
    // slot a new client immediately claims can never observe a stale
    // high-water mark left over from whoever held it before.
    void ReleaseClientSlot(uint32_t clientId);

    // Test-only introspection: the current sampleTime -> ring-position mapping
    // ReadMic is using (see micState_ below). Exposed so tests can assert that
    // a given ReadMic call did or did not resync, without duplicating ReadMic's
    // private state.
    int64_t DebugMicOffset() const;

private:
    SharedLayout* layout_ = nullptr;
    std::string name_;

    // micSynced_ (a bool) and micOffset_ (the sampleTime -> ring-position
    // mapping) used to be two separate atomics. ReadMic can be called
    // concurrently from more than one IO thread for the same device (one
    // per client), so two independent atomics allow a torn read: a thread
    // could observe "synced" paired with an offset from before or after
    // that flag flipped. They're packed into one 64-bit atomic instead -
    // bit 63 is the synced flag, the remaining 63 bits are the
    // sign-extended offset - so every read/write of *this* pair is a
    // single atomic op with no torn-read window.
    //
    // Every other atomic below (maxFramesSeen_, underrunStreak_,
    // underrunDeficit_, lastMicW_, silenceFloor_, the margin-trim window,
    // and each per-client servedEnd) is an independent, unpacked atomic:
    // a race between two concurrent ReadMic calls can update them out of
    // the order either call observed. This is intentional, not an
    // oversight - they're heuristics/bookkeeping (when to resync, when to
    // trim, how far a specific client has been served), not correctness
    // invariants for the audio content itself, so the worst a race can do
    // is make a resync/trim decision a cycle or two early or late. Notably,
    // wAdvancing (derived from lastMicW_) only reflects whether write_pos
    // moved since the *immediately preceding* ReadMic call from *any*
    // client: with two or more clients calling within what's logically the
    // same cycle, only the first caller (whichever that happens to be) can
    // observe it as true, and underrunStreak_/underrunDeficit_ likewise
    // count ReadMic *calls*, not distinct time cycles - so with N clients
    // attached, the sustained-underrun thresholds (see ReadMic) can be
    // reached in effect ~N times faster than a single-client reading of
    // "3 consecutive cycles" suggests. This is an accepted imprecision
    // (the mechanism still only ever self-corrects, never mis-serves
    // audio) rather than added cycle-keying complexity for marginal
    // benefit.
    std::atomic<uint64_t> micState_{0};

    // High-water mark of `frames` across every ReadMic call on this
    // region since the last ResetIOStats() (see fix 2 in ReadMic's doc
    // comment above).
    std::atomic<uint32_t> maxFramesSeen_{0};

    // Consecutive-cycle and accumulated-frame counters used to detect a
    // sustained (as opposed to one-off) underrun - see ReadMic.
    std::atomic<uint32_t> underrunStreak_{0};
    std::atomic<uint32_t> underrunDeficit_{0};

    // write_pos observed on the previous ReadMic call (any client), used to
    // tell whether the write edge is actively advancing right now. Gates
    // the sustained-underrun resync above: re-anchoring while the writer is
    // merely frozen (not dead - heartbeat still fresh) would land on stale
    // audio and replay it.
    std::atomic<uint64_t> lastMicW_{0};

    // The highest write_pos ever observed while ReadMic was in its
    // silence branch (stale/zero heartbeat, or not enough runway yet).
    // This marks a hard floor: once the app resumes after such a gap
    // (a coordinator hand-off, or a new app process entirely) and the
    // next resync re-anchors, nothing at or before this floor may ever be
    // served as real audio again - even though this specific reader may
    // never have actually received it (so the per-client guard below
    // wouldn't by itself catch it), it belongs to a session that's over,
    // and serving it would mean playing back audio from before the gap.
    std::atomic<uint64_t> silenceFloor_{0};

    // Per-client replay guard: the highest absolute mic-ring position ever
    // served *to this specific client* as real (non-zero-filled) audio.
    // Keyed by aspl::Client::GetClientID() (see ReadMic's `clientId`
    // parameter). A resync that re-anchors backward relative to what a
    // given client has already been served must not re-serve that range to
    // *that* client - but must not silence a *different* client that
    // simply hasn't reached as far yet (this is the normal, legitimate
    // case of multiple clients with different IO cycle sizes, or different
    // per-client sampleTime cadences, sharing one anchor). Sized generously
    // for how many processes could plausibly read "RoomMesh Microphone" at
    // once; look-up is a short linear scan (realtime-safe: no locks, no
    // allocation).
    //
    // Slots are released explicitly (see ReleaseClientSlot(), called from
    // IOStateHandler::OnRemoveClient) rather than only ever being claimed -
    // without that, a long-running coreaudiod process would eventually see
    // more than kMaxTrackedMicClients distinct clients over its lifetime
    // and permanently run out of slots. As a fallback (in case some client
    // disconnects through a path that doesn't reach OnRemoveClient), a slot
    // idle for over ~1s is also eligible to be reclaimed by a new client.
    // If every slot is nonetheless in use by a still-active client and a
    // new one shows up, that new client's reads simply aren't guarded
    // (fail open to serving audio rather than wrongly silencing or wrongly
    // guarding an unrelated client) - kMaxTrackedMicClients is set well
    // above any realistic number of simultaneous consumers of this virtual
    // device, so this should never be reached in practice.
    static constexpr uint32_t kNoClient = 0xFFFFFFFFu;
    static constexpr int kMaxTrackedMicClients = 16;
    static constexpr uint64_t kClientSlotIdleNs = 1'000'000'000ull;
    struct ClientServedEnd {
        std::atomic<uint32_t> clientId{kNoClient};
        std::atomic<uint64_t> servedEnd{0};
        std::atomic<uint64_t> lastSeenNs{0};
    };
    ClientServedEnd micClientServed_[kMaxTrackedMicClients];
    // Finds this client's slot, claiming a free (or long-idle) one on first
    // use and resetting its servedEnd to 0. Returns nullptr if the table is
    // full of still-active clients and clientId isn't already tracked (see
    // kMaxTrackedMicClients above).
    ClientServedEnd* FindOrCreateClientSlot(uint32_t clientId, uint64_t nowNs);
    // Resets every mic-side tracking field (micState_ through
    // micClientServed_) to its fresh-construction state. Called from
    // Create() so a SharedRegion reused across more than one shm region
    // (e.g. a driver re-init) never carries stale bookkeeping - an offset,
    // a client's high-water mark, an in-progress trim window - into a
    // brand new region whose write_pos starts back at 0.
    void ResetMicTrackingState();

    // One-time forward anchor trim (see ReadMic): tracks how long the
    // margin between the write edge and what's actually being served has
    // stayed above the normal readBehind depth, so a resync that (for
    // whatever reason - e.g. against a margin sized for a client that's
    // since disconnected) left the anchor deeper than necessary doesn't
    // carry that extra latency forever.
    std::atomic<uint64_t> marginWindowStartNs_{0};  // 0 = no window currently open
    std::atomic<uint64_t> marginWindowMinGap_{0};

    int lastErrno_ = 0;
    const char* lastStep_ = nullptr;
};

}  // namespace roommesh
