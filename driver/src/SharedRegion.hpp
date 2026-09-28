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
    // Reads at least max(kMicLatencyFrames, frames + 480) frames behind the app's write edge
    // (see ReadMic's `readBehind` in SharedRegion.cpp), resyncing that anchor only when the
    // app has clearly (re)started or drifted far out of range in either direction. A plain,
    // brief underrun (pos + frames > write edge) is served partially with a zero-filled tail
    // rather than resyncing backward, which would replay already-served audio.
    void ReadMic(double sampleTime, float* out, uint32_t frames, uint64_t nowNs);
    // Store the mixed RoomMesh Speaker output (interleaved `channels`) downmixed to mono.
    void WriteSpeaker(const float* interleaved, uint32_t frames, uint32_t channels, uint64_t nowNs);
    void Heartbeat(uint64_t nowNs);

    // Test-only introspection: the current sampleTime -> ring-position mapping
    // ReadMic is using (see micOffset_ below). Exposed so tests can assert that
    // a given ReadMic call did or did not resync, without duplicating ReadMic's
    // private state.
    int64_t DebugMicOffset() const { return micOffset_.load(std::memory_order_relaxed); }

private:
    SharedLayout* layout_ = nullptr;
    std::string name_;
    std::atomic<bool> micSynced_{false};
    std::atomic<int64_t> micOffset_{0};
    int lastErrno_ = 0;
    const char* lastStep_ = nullptr;
};

}  // namespace roommesh
