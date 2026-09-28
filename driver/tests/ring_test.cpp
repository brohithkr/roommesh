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
    std::puts("ring_test PASS");
    return 0;
}
