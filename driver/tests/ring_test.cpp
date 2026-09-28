#include "SharedRegion.hpp"
#include <algorithm>
#include <cassert>
#include <cstdio>
#include <string>
#include <unistd.h>

using namespace roommesh;

// Mirrors SharedRegion::ReadMic's readBehind = max(kMicLatencyFrames, frames + 480).
static uint64_t readBehindFor(uint32_t frames) {
    return std::max<uint64_t>(kMicLatencyFrames, static_cast<uint64_t>(frames) + 480);
}

static void writeMic(SharedLayout* l, float v, uint32_t n, uint64_t now) {
    uint64_t w = l->mic.h.write_pos.load();
    for (uint32_t i = 0; i < n; i++) l->mic.samples[(w + i) & kRingMask] = v;
    l->mic.h.write_pos.store(w + n);
    l->header.app_heartbeat_ns.store(now);
}

// Like writeMic, but stores each sample's own absolute (unwrapped) write
// position as its value, so a reader can later verify exactly which
// position it read from just by inspecting sample values - useful for
// detecting an unexpected resync or a stale/replayed sample.
static void writeMicIndexed(SharedLayout* l, uint32_t n, uint64_t now) {
    uint64_t w = l->mic.h.write_pos.load();
    for (uint32_t i = 0; i < n; i++) l->mic.samples[(w + i) & kRingMask] = static_cast<float>(w + i);
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
    // readBehind for a 512-frame IO cycle is max(960, 512+480) = 992, not
    // the bare kMicLatencyFrames (fix 2: read-behind scales with IO size).
    assert(l->mic.h.read_pos.load() == total - readBehindFor(512) + 512);

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
    // readBehind = max(960, 512+480) = 992; establishes sync: offset = (4800-992)-1000 = 2808
    r.ReadMic(1000, out, 512, now);
    for (float f : out) assert(f == 0.6f);
    const uint64_t readPosAfterFirst = l->mic.h.read_pos.load();
    assert(readPosAfterFirst == 4800 - readBehindFor(512) + 512);

    // sampleTime jumps far backward; with the old offset this would compute
    // a deeply negative pos (w - pos >> readBehind + 1440), which must
    // trigger a resync back to near the write edge rather than reading
    // garbage or an out-of-range index.
    r.ReadMic(-1'000'000, out, 512, now);
    for (float f : out) assert(f == 0.6f);
    // The resynced position lands back at the same w - readBehind
    // edge as before, so read_pos must not have moved backward.
    assert(l->mic.h.read_pos.load() == readPosAfterFirst);

    r.Destroy();
}

// Fix (3): if the device keeps asking for later sampleTimes without the app
// writing any more frames (a brief underrun), pos + frames eventually
// exceeds the write edge w. Previously this forced a resync back to the
// write edge - but re-anchoring backward there replays already-served audio
// (a buzz). Instead ReadMic must serve whatever actually exists and
// zero-fill only the unwritten tail, leaving the anchor (mic offset) exactly
// where it was, so a steady app with small jitter never glitches and a
// genuine, brief underrun doesn't add a discontinuity on top of silence.
static void underrunZeroFillTest() {
    const std::string name = "/rmtest.underrun." + std::to_string(getpid());
    SharedRegion r;
    assert(r.Create(name.c_str()));
    SharedLayout* l = r.layout();
    const uint64_t now = 7'000'000'000;

    writeMic(l, 0.3f, 4800, now);  // w = 4800
    float out[512];
    // readBehind = max(960, 512+480) = 992; establishes sync: offset = (4800-992)-1000 = 2808
    r.ReadMic(1000, out, 512, now);
    for (float f : out) assert(f == 0.3f);
    const int64_t offset0 = r.DebugMicOffset();
    const uint64_t readPosAfterFirst = l->mic.h.read_pos.load();
    assert(readPosAfterFirst == 4800 - readBehindFor(512) + 512);

    // No further writes happen. sampleTime 1600 -> pos = 1600+2808 = 4408,
    // which is still inside the written range (< w = 4800) but pos+frames =
    // 4920 > w: a mild underrun. 392 frames exist (4408..4799); 120 don't.
    // This is nowhere near either resync threshold (pos > w+readBehind, or
    // w - pos > readBehind+1440), so the anchor must not move.
    r.ReadMic(1600, out, 512, now);
    assert(r.DebugMicOffset() == offset0);  // anchor did not re-anchor backward
    for (uint32_t i = 0; i < 392; i++) assert(out[i] == 0.3f);  // real, previously-written audio
    for (uint32_t i = 392; i < 512; i++) assert(out[i] == 0.f);  // zero-filled tail, not replayed audio
    assert(l->mic.h.read_pos.load() == 4408 + 512);

    r.Destroy();
}

// Fix (1): after an app stall, a burst of queued writes can land all at
// once and push the write edge far ahead of where a still-"synced" reader
// is tracking. That gap must not be allowed to persist and add unbounded
// latency: ReadMic must resync once the reader falls more than
// readBehind + 1440 frames behind the write edge, not only near the full
// ring size (the previous bound).
static void burstAfterStallBoundedLatencyTest() {
    const std::string name = "/rmtest.burst." + std::to_string(getpid());
    SharedRegion r;
    assert(r.Create(name.c_str()));
    SharedLayout* l = r.layout();
    const uint64_t now = 11'000'000'000;

    writeMic(l, 0.1f, 4800, now);  // w = 4800
    float out[512];
    // readBehind = 992; establishes sync: offset = (4800-992)-1000 = 2808
    r.ReadMic(1000, out, 512, now);
    for (float f : out) assert(f == 0.1f);

    // Burst: 10000 more frames land in one shot (queued audio flushed right
    // after a stall). w jumps from 4800 to 14800 - far beyond
    // readBehind(992)+1440 = 2432, but still well short of the old bound
    // (kRingFrames - 4800 = 27968), so the old code would never resync here
    // and would leave the reader ~10000 frames (~208 ms) behind forever.
    writeMic(l, 0.9f, 10000, now);  // w = 14800

    // Continue the reader's sampleTime exactly as if nothing had happened.
    r.ReadMic(1512, out, 512, now);
    // With the stale offset this would read pos = 1512+2808 = 4320, i.e.
    // 14800-4320 = 10480 frames behind the new write edge - beyond the
    // readBehind+1440 bound, so it must resync back near the edge instead
    // and serve the fresh (0.9) audio.
    for (float f : out) assert(f == 0.9f);
    const uint64_t readPos = l->mic.h.read_pos.load();
    // Bounded: the served position must land within readBehind of the new
    // write edge, not 10000+ frames behind it.
    assert(14800 - readPos <= readBehindFor(512));

    r.Destroy();
}

// Fix (2): a reader with a larger IO cycle (e.g. 1024 frames) must not
// resync every cycle just because kMicLatencyFrames (960) is close to its
// buffer size - readBehind scales to frames + 480 instead. This simulates
// the app's write cadence as an uneven ("sawtooth") sequence of small
// writes against a steady 1024-frame reader and checks that, after the
// initial sync, the mic offset never changes (zero resyncs) and every
// sample read matches exactly what was written - no zero-fill glitches, no
// stale replay - even though the write/read cycle boundaries never align.
static void largeReaderStableNoResyncTest() {
    const std::string name = "/rmtest.stable." + std::to_string(getpid());
    SharedRegion r;
    assert(r.Create(name.c_str()));
    SharedLayout* l = r.layout();
    const uint64_t now = 12'000'000'000;

    // Pre-buffer so the first ReadMic call has a runway to sync against.
    writeMicIndexed(l, 4800, now);

    const uint32_t frames = 1024;
    float out[frames];
    double sampleTime = 1000;

    // readBehind = max(960, 1024+480) = 1504.
    r.ReadMic(sampleTime, out, frames, now);
    const int64_t offset0 = r.DebugMicOffset();
    for (uint32_t i = 0; i < frames; i++) assert(out[i] == static_cast<float>(sampleTime + offset0 + i));

    // Chunk sizes vary cycle to cycle (simulating the app's write-timing
    // jitter/sawtooth) but always sum to 1024 - the same as `frames` - so
    // the write edge stays a constant readBehind ahead of the reader
    // throughout, the way a real app writing at the nominal sample rate
    // would (only the timing within each ~10ms slot saws back and forth,
    // not the long-run throughput).
    const uint32_t chunkPattern[4][2] = {{480, 544}, {500, 524}, {450, 574}, {520, 504}};
    for (int cycle = 0; cycle < 200; cycle++) {
        const uint32_t* pair = chunkPattern[cycle % 4];
        writeMicIndexed(l, pair[0], now);
        writeMicIndexed(l, pair[1], now);

        sampleTime += frames;
        r.ReadMic(sampleTime, out, frames, now);

        assert(r.DebugMicOffset() == offset0);  // no resync after the first sync
        const double pos = sampleTime + offset0;
        for (uint32_t i = 0; i < frames; i++) assert(out[i] == static_cast<float>(pos + i));
    }

    r.Destroy();
}

// Fix (4): the app stores app_heartbeat_ns = 0 the instant it stops being
// the room's coordinator, rather than leaving the last real timestamp
// there. Reads must silence immediately when that happens - not only after
// a 200ms timeout - because nowNs > beat + 200ms is trivially true for any
// nowNs when beat == 0.
static void heartbeatZeroSilenceTest() {
    const std::string name = "/rmtest.heartbeatzero." + std::to_string(getpid());
    SharedRegion r;
    assert(r.Create(name.c_str()));
    SharedLayout* l = r.layout();
    const uint64_t now = 13'000'000'000;

    writeMic(l, 0.7f, 4800, now);  // writeMic also sets app_heartbeat_ns = now
    float out[512];
    r.ReadMic(1000, out, 512, now);
    for (float f : out) assert(f == 0.7f);  // sanity: audio flows while heartbeat is fresh

    l->header.app_heartbeat_ns.store(0);  // app relinquished the coordinator role
    r.ReadMic(1512, out, 512, now);  // same `now` - i.e. immediately, no timeout elapsed
    for (float f : out) assert(f == 0.f);

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

    // app writes 100 ms of 0.5 → driver reads readBehind(512) = max(960, 512+480) = 992
    // frames behind the write edge (fix 2: read-behind scales with IO cycle size).
    const uint64_t now = 2'000'000'000;
    writeMic(l, 0.5f, 4800, now);
    r.ReadMic(1000, out, 512, now);
    for (float f : out) assert(f == 0.5f);
    assert(l->mic.h.read_pos.load() == 4800 - readBehindFor(512) + 512);
    // next cycle continues contiguously (same offset); w is now 4800+512=5312
    writeMic(l, 0.25f, 512, now);
    r.ReadMic(1512, out, 512, now);
    assert(l->mic.h.read_pos.load() == 5312 - readBehindFor(512) + 512);
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
    underrunZeroFillTest();
    burstAfterStallBoundedLatencyTest();
    largeReaderStableNoResyncTest();
    heartbeatZeroSilenceTest();

    std::puts("ring_test PASS");
    return 0;
}
