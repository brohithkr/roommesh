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

    // Serve `frames` mono samples for the RoomMesh Microphone IO cycle at device sample time `sampleTime`.
    // Every client in the same cycle passes the same sampleTime and receives the same audio.
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
    // already-served audio.
    void ReadMic(double sampleTime, float* out, uint32_t frames, uint64_t nowNs);
    // Store the mixed RoomMesh Speaker output (interleaved `channels`) downmixed to mono.
    void WriteSpeaker(const float* interleaved, uint32_t frames, uint32_t channels, uint64_t nowNs);
    void Heartbeat(uint64_t nowNs);

    // Resets the cross-client IO-cycle-size high-water mark (maxFramesSeen_)
    // that sizes ReadMic's readBehind margin. Called from
    // IOStateHandler::OnStartIO when the mic device starts IO, so a
    // high-water mark left over from a previous IO session (e.g. one that
    // briefly had a larger-buffer client attached) can't linger and
    // over-deepen the margin forever.
    void ResetIOStats();

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
    // sign-extended offset - so every read/write is a single atomic op.
    // See PackMicState/UnpackMicSynced/UnpackMicOffset in SharedRegion.cpp.
    std::atomic<uint64_t> micState_{0};

    // High-water mark of `frames` across every ReadMic call on this
    // region since the last ResetIOStats() (see fix 2 in ReadMic's doc
    // comment above). Relaxed atomic max, not synchronization.
    std::atomic<uint32_t> maxFramesSeen_{0};

    // Consecutive-cycle and accumulated-frame counters used to detect a
    // sustained (as opposed to one-off) underrun - see ReadMic.
    std::atomic<uint32_t> underrunStreak_{0};
    std::atomic<uint32_t> underrunDeficit_{0};

    // Highest absolute mic-ring position ever served to a reader as real
    // (non-zero-filled) audio, and the offset that was in effect when it
    // was last advanced. Together these guarantee that re-anchoring
    // backward (any resync branch) never re-serves - replays - audio a
    // listener has already heard, while still letting different clients
    // legitimately share overlapping reads at the same sampleTime under a
    // stable (not just-changed) offset; see ReadMic.
    std::atomic<uint64_t> servedRealEnd_{0};
    std::atomic<int64_t> servedRealOffset_{0};

    // write_pos observed on the previous ReadMic call (any client), used to
    // tell whether the write edge is actively advancing right now. Gates
    // the sustained-underrun resync above: re-anchoring while the writer is
    // merely frozen (not dead - heartbeat still fresh) would land on stale
    // audio and replay it.
    std::atomic<uint64_t> lastMicW_{0};

    int lastErrno_ = 0;
    const char* lastStep_ = nullptr;
};

}  // namespace roommesh
