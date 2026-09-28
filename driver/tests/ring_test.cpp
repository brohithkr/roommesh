#include "SharedRegion.hpp"
#include <cassert>
#include <cstdio>
#include <string>
#include <unistd.h>

using namespace roommesh;

static void writeMic(SharedLayout* l, float v, uint32_t n, uint64_t now) {
    uint64_t w = l->mic.h.write_pos.load();
    for (uint32_t i = 0; i < n; i++) l->mic.samples[(w + i) & kRingMask] = v;
    l->mic.h.write_pos.store(w + n);
    l->header.app_heartbeat_ns.store(now);
}

// Writing more than kRingFrames total keeps the ring's monotonic write_pos
// counter growing past 32768 while the underlying samples[] index wraps via
// `& kRingMask`. Verify ReadMic still serves the most recently written data
// (not stale data left behind by an earlier lap around the ring) once the
// write edge has wrapped at least once.
static void ringWrapAroundTest() {
    const std::string name = "/rmtest.wrap." + std::to_string(getpid());
    SharedRegion r;
    assert(r.Create(name.c_str()));
    SharedLayout* l = r.layout();
    const uint64_t now = 5'000'000'000;

    uint64_t total = 0;
    while (total < kRingFrames + 4800) {
        writeMic(l, 0.11f, 4800, now);
        total += 4800;
    }
    assert(total > kRingFrames);  // confirms we actually wrapped at least once
    writeMic(l, 0.42f, 4800, now);  // distinct value written right after the wrap
    total += 4800;

    float out[512];
    r.ReadMic(static_cast<double>(total), out, 512, now);
    for (float f : out) assert(f == 0.42f);
    assert(l->mic.h.read_pos.load() == total - kMicLatencyFrames + 512);

    r.Destroy();
}

// A large backward jump in the device's sampleTime (e.g. the HAL restarting
// a stream) must force a resync via the "w - pos too large" branch, not
// silently keep reading from a now-meaningless position far outside the
// ring's live window.
static void backwardSampleTimeJumpTest() {
    const std::string name = "/rmtest.backjump." + std::to_string(getpid());
    SharedRegion r;
    assert(r.Create(name.c_str()));
    SharedLayout* l = r.layout();
    const uint64_t now = 6'000'000'000;

    writeMic(l, 0.6f, 4800, now);  // w = 4800
    float out[512];
    r.ReadMic(1000, out, 512, now);  // establishes sync: offset = (4800-960)-1000 = 2840
    for (float f : out) assert(f == 0.6f);
    const uint64_t readPosAfterFirst = l->mic.h.read_pos.load();
    assert(readPosAfterFirst == 4800 - kMicLatencyFrames + 512);

    // sampleTime jumps far backward; with the old offset this would compute
    // a deeply negative pos (w - pos >> kRingFrames - 4800), which must
    // trigger a resync back to near the write edge rather than reading
    // garbage or an out-of-range index.
    r.ReadMic(-1'000'000, out, 512, now);
    for (float f : out) assert(f == 0.6f);
    // The resynced position lands back at the same w - kMicLatencyFrames
    // edge as before, so read_pos must not have moved backward.
    assert(l->mic.h.read_pos.load() == readPosAfterFirst);

    r.Destroy();
}

// If the device keeps asking for later sampleTimes without the app writing
// any more frames (an underrun), pos + frames eventually exceeds the write
// edge w. That must force a resync back to the write edge instead of
// reading past what has actually been written.
static void underrunResyncTest() {
    const std::string name = "/rmtest.underrun." + std::to_string(getpid());
    SharedRegion r;
    assert(r.Create(name.c_str()));
    SharedLayout* l = r.layout();
    const uint64_t now = 7'000'000'000;

    writeMic(l, 0.3f, 4800, now);  // w = 4800
    float out[512];
    r.ReadMic(1000, out, 512, now);  // establishes sync: offset = (4800-960)-1000 = 2840
    for (float f : out) assert(f == 0.3f);
    const uint64_t readPosAfterFirst = l->mic.h.read_pos.load();
    assert(readPosAfterFirst == 4800 - kMicLatencyFrames + 512);

    // No further writes happen, but sampleTime advances: with the existing
    // offset, pos + frames = (3000 + 2840) + 512 = 6352 > w (4800). Must
    // resync back to the write edge rather than serving frames the app
    // hasn't produced yet.
    r.ReadMic(3000, out, 512, now);
    for (float f : out) assert(f == 0.3f);
    assert(l->mic.h.read_pos.load() == readPosAfterFirst);

    r.Destroy();
}

int main() {
    std::string name = "/rmtest.cpp." + std::to_string(getpid());
    SharedRegion r;
    assert(r.Create(name.c_str()));
    SharedLayout* l = r.layout();
    assert(l->header.magic == kMagic && l->header.version == kVersion && l->header.ring_frames == kRingFrames);
    assert(l->header.generation.load() >= 1);

    float out[512];
    // no app writing → silence
    for (float& f : out) f = 1.f;
    r.ReadMic(0, out, 512, 1'000'000'000);
    for (float f : out) assert(f == 0.f);

    // app writes 100 ms of 0.5 → driver reads 20 ms behind the write edge
    const uint64_t now = 2'000'000'000;
    writeMic(l, 0.5f, 4800, now);
    r.ReadMic(1000, out, 512, now);
    for (float f : out) assert(f == 0.5f);
    assert(l->mic.h.read_pos.load() == 4800 - kMicLatencyFrames + 512);
    // next cycle continues contiguously (same offset)
    writeMic(l, 0.25f, 512, now);
    r.ReadMic(1512, out, 512, now);
    assert(l->mic.h.read_pos.load() == 4800 - kMicLatencyFrames + 1024);
    // stale app heartbeat → silence
    r.ReadMic(2024, out, 512, now + 500'000'000);
    for (float f : out) assert(f == 0.f);

    // speaker: stereo downmix
    float st[8] = {1, 0, 0.5f, 0.5f, 0, 0, -1, 1};
    r.WriteSpeaker(st, 4, 2, now);
    assert(l->speaker.h.write_pos.load() == 4);
    assert(l->speaker.samples[0] == 0.5f && l->speaker.samples[1] == 0.5f && l->speaker.samples[3] == 0.f);
    assert(l->speaker.h.write_host_ns.load() == now);

    r.Heartbeat(now);
    assert(l->header.driver_heartbeat_ns.load() == now);
    r.Destroy();

    ringWrapAroundTest();
    backwardSampleTimeJumpTest();
    underrunResyncTest();

    std::puts("ring_test PASS");
    return 0;
}
