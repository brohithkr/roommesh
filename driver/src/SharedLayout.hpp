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
// Minimum distance (in frames) the driver reads behind the app's write
// edge (~20 ms). This is a floor, not a fixed lag: SharedRegion::ReadMic
// scales the actual read-behind distance up for larger IO cycle sizes
// (readBehind = max(kMicLatencyFrames, maxFramesSeen + kMicLatencyFrames))
// so clients with bigger buffers (e.g. 1024+ frames) don't resync every
// cycle, and so one client's margin isn't sized too shallow for another,
// larger client sharing the same anchor. The extra kMicLatencyFrames of
// headroom (rather than a smaller one) was sized against a simulation of
// the app's real write cadence (~2ms ticks + timing jitter): 0 zero-filled
// cycles across 1440 simulated runs.
constexpr uint64_t kMicLatencyFrames = 960;

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

// Field-offset assertions mirroring the Rust-side layout test
// (core/roommesh-core/src/audio/shared_layout.rs). This struct is a shared
// memory-mapped ABI between the driver (this file) and the app: any
// unintentional offset shift here would silently desync the two sides.
// std::atomic<uint64_t>/<uint32_t> as used here are trivial wrappers that
// remain standard-layout under libc++/libstdc++, but some toolchains still
// warn on offsetof through them; the warning is suppressed locally rather
// than dropping the check.
#if defined(__clang__) || defined(__GNUC__)
#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Winvalid-offsetof"
#endif
static_assert(offsetof(SharedHeader, generation) == 16, "SharedHeader::generation must sit at byte 16");
static_assert(offsetof(SharedLayout, mic) == 64, "SharedLayout::mic must sit at byte 64");
static_assert(offsetof(SharedLayout, speaker) == 131200, "SharedLayout::speaker must sit at byte 131200");
#if defined(__clang__) || defined(__GNUC__)
#pragma GCC diagnostic pop
#endif

}  // namespace roommesh
