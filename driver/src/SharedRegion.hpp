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

    // Serve `frames` mono samples for the RoomMesh Microphone IO cycle at device sample time `sampleTime`.
    // Every client in the same cycle passes the same sampleTime and receives the same audio.
    void ReadMic(double sampleTime, float* out, uint32_t frames, uint64_t nowNs);
    // Store the mixed RoomMesh Speaker output (interleaved `channels`) downmixed to mono.
    void WriteSpeaker(const float* interleaved, uint32_t frames, uint32_t channels, uint64_t nowNs);
    void Heartbeat(uint64_t nowNs);

private:
    SharedLayout* layout_ = nullptr;
    std::string name_;
    std::atomic<bool> micSynced_{false};
    std::atomic<int64_t> micOffset_{0};
};

}  // namespace roommesh
