// Shared-memory contract between the RoomMesh HAL driver and the RoomMesh app.
// Mirror: core/roommesh-core/src/audio/shared_layout.rs — keep byte-for-byte identical.
#pragma once
#include <atomic>
#include <cstddef>
#include <cstdint>

namespace roommesh {

constexpr const char* kShmName = "/roommesh.v1";
constexpr uint32_t kMagic = 0x524D5348;  // 'RMSH'
constexpr uint32_t kVersion = 1;
constexpr uint32_t kSampleRate = 48000;
constexpr uint32_t kRingFrames = 32768;  // power of two (~680 ms)
constexpr uint64_t kRingMask = kRingFrames - 1;
constexpr uint64_t kMicLatencyFrames = 960;  // driver reads 20 ms behind the app's write edge

struct SharedHeader {
    uint32_t magic;
    uint32_t version;
    uint32_t sample_rate;
    uint32_t ring_frames;
    std::atomic<uint64_t> generation;
    std::atomic<uint64_t> driver_heartbeat_ns;
    std::atomic<uint64_t> app_heartbeat_ns;
    std::atomic<uint32_t> mic_clients;
    std::atomic<uint32_t> speaker_clients;
    uint64_t reserved[2];
};

struct RingHeader {
    std::atomic<uint64_t> write_pos;
    std::atomic<uint64_t> write_host_ns;
    std::atomic<uint64_t> read_pos;
    std::atomic<uint64_t> read_host_ns;
    uint64_t reserved[4];
};

struct Ring {
    RingHeader h;
    float samples[kRingFrames];
};

struct SharedLayout {
    SharedHeader header;
    Ring mic;      // app writes, driver reads  → "RoomMesh Microphone"
    Ring speaker;  // driver writes, app reads  ← "RoomMesh Speaker"
};

static_assert(std::atomic<uint64_t>::is_always_lock_free, "need lock-free 64-bit atomics");
static_assert(sizeof(SharedHeader) == 64);
static_assert(sizeof(RingHeader) == 64);
static_assert(sizeof(Ring) == 64 + 4 * kRingFrames);
static_assert(sizeof(SharedLayout) == 262336);

}  // namespace roommesh
