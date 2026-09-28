#include "SharedRegion.hpp"
#include <algorithm>
#include <cassert>
#include <cstdio>
#include <random>
#include <string>
#include <unistd.h>
#include <vector>

using namespace roommesh;

// Mirrors SharedRegion::ReadMic's readBehind = max(kMicLatencyFrames, maxFramesSeen + kMicLatencyFrames).
// Each test below uses a fresh SharedRegion with only one client frame size in play (unless
// noted), so maxFramesSeen == that call's own `frames` and this simplifies to a per-frames value.
static uint64_t readBehindFor(uint32_t frames) {
    return std::max<uint64_t>(kMicLatencyFrames, static_cast<uint64_t>(frames) + kMicLatencyFrames);
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

// Fill the entire mic ring with a distinct sentinel value before any real
// writes. Create() already zero-fills the region, which means a bug that
// served unwritten/stale ring memory instead of an explicit zero for
// positions at or beyond the write edge could go unnoticed - it would read
// back as 0.f either way. Overwriting with a value no test ever writes or
// expects (9.f) closes that blind spot: any leak of stale ring memory into
// the output shows up as a distinct, unmistakable 9.f rather than silently
// matching an "expected" zero.
static void sentinelFillRing(SharedLayout* l) {
    for (uint64_t i = 0; i < kRingFrames; i++) l->mic.samples[i] = 9.f;
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
    sentinelFillRing(l);
    const uint64_t now = 6'000'000'000;

    writeMic(l, 0.6f, 4800, now);  // w = 4800
    float out[512];
    // readBehind = max(960, 512+960) = 1472; establishes sync: offset = (4800-1472)-1000 = 2328
    r.ReadMic(1000, out, 512, now);
    for (float f : out) assert(f == 0.6f);
    const int64_t offset0 = r.DebugMicOffset();
    const uint64_t readPosAfterFirst = l->mic.h.read_pos.load();
    assert(readPosAfterFirst == 4800 - readBehindFor(512) + 512);

    // sampleTime jumps far backward; with the old offset this would compute
    // a deeply negative pos (w - pos >> readBehind + 1440), which must
    // trigger a resync via the "w far ahead of pos" bound rather than
    // silently reading garbage or an out-of-range index.
    r.ReadMic(-1'000'000, out, 512, now);
    // The resync recomputes off from the *current* (very negative) sampleTime
    // against the same w, so the resulting position happens to land back on
    // exactly the same [3328, 3840) range this reader already served above
    // (offset0's pos). Fix (1)'s servedRealEnd_ guard must recognize that
    // range as already-delivered and zero-fill it rather than replay it -
    // re-serving the same 512 samples a second time would be exactly the
    // kind of buzz this guard exists to prevent.
    assert(r.DebugMicOffset() != offset0);  // a resync did happen (the offset itself changed)
    for (float f : out) assert(f == 0.f);
    for (float f : out) assert(f != 9.f);  // not stale/unwritten ring memory either
    // Nothing new was actually served (it was all masked), so read_pos must
    // not have moved backward, but also gains nothing beyond what fix (1)'s
    // bookkeeping already advanced it to.
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
    sentinelFillRing(l);
    const uint64_t now = 7'000'000'000;

    writeMic(l, 0.3f, 4800, now);  // w = 4800
    float out[512];
    // readBehind = max(960, 512+960) = 1472; establishes sync: offset = (4800-1472)-1000 = 2328
    r.ReadMic(1000, out, 512, now);
    for (float f : out) assert(f == 0.3f);
    const int64_t offset0 = r.DebugMicOffset();
    const uint64_t readPosAfterFirst = l->mic.h.read_pos.load();
    assert(readPosAfterFirst == 4800 - readBehindFor(512) + 512);

    // No further writes happen. sampleTime 2200 -> pos = 2200+2328 = 4528,
    // which is still inside the written range (< w = 4800) but pos+frames =
    // 5040 > w: a mild, one-cycle underrun. 272 frames exist (4528..4799);
    // 240 don't. This is nowhere near either static resync threshold
    // (pos > w+readBehind, or w - pos > readBehind+1440) and is only the
    // first underrun cycle (fix 1 requires 3 consecutive, or >960 frames
    // accumulated, before forcing a resync), so the anchor must not move.
    r.ReadMic(2200, out, 512, now);
    assert(r.DebugMicOffset() == offset0);  // anchor did not re-anchor backward
    for (uint32_t i = 0; i < 272; i++) assert(out[i] == 0.3f);  // real, previously-written audio
    for (uint32_t i = 272; i < 512; i++) assert(out[i] == 0.f);  // zero-filled tail, not replayed audio
    assert(l->mic.h.read_pos.load() == 4528 + 512);

    r.Destroy();
}

// Fix (1), original scenario: after an app stall, a burst of queued writes
// can land all at once and push the write edge far ahead of where a
// still-"synced" reader is tracking. That gap must not be allowed to
// persist and add unbounded latency: ReadMic must resync once the reader
// falls more than readBehind + 1440 frames behind the write edge, not only
// near the full ring size (the previous bound).
static void burstAfterStallBoundedLatencyTest() {
    const std::string name = "/rmtest.burst." + std::to_string(getpid());
    SharedRegion r;
    assert(r.Create(name.c_str()));
    SharedLayout* l = r.layout();
    const uint64_t now = 11'000'000'000;

    writeMic(l, 0.1f, 4800, now);  // w = 4800
    float out[512];
    // readBehind = 1472; establishes sync: offset = (4800-1472)-1000 = 2328
    r.ReadMic(1000, out, 512, now);
    for (float f : out) assert(f == 0.1f);

    // Burst: 10000 more frames land in one shot (queued audio flushed right
    // after a stall). w jumps from 4800 to 14800 - far beyond
    // readBehind(1472)+1440 = 2912, but still well short of the old bound
    // (kRingFrames - 4800 = 27968), so the old code would never resync here
    // and would leave the reader ~10000 frames (~208 ms) behind forever.
    writeMic(l, 0.9f, 10000, now);  // w = 14800

    // Continue the reader's sampleTime exactly as if nothing had happened.
    r.ReadMic(1512, out, 512, now);
    // With the stale offset this would read pos = 1512+2328 = 3840, i.e.
    // 14800-3840 = 10960 frames behind the new write edge - beyond the
    // readBehind+1440 bound, so it must resync back near the edge instead
    // and serve the fresh (0.9) audio.
    for (float f : out) assert(f == 0.9f);
    const uint64_t readPos = l->mic.h.read_pos.load();
    // Bounded: the served position must land within readBehind of the new
    // write edge, not 10000+ frames behind it.
    assert(14800 - readPos <= readBehindFor(512));

    r.Destroy();
}

// Fix (1) helper, reused by the three sustained-underrun tests below.
// Verifies every non-zero sample in `out` (for a call that read at absolute
// position `pos`) is exactly the value that position's writer stored there
// (catching stale/wrong reads), and - the actual point of these tests -
// that no absolute position is ever served as real audio more than once
// (catching a replay/buzz from re-anchoring backward over already-served
// audio). Pass the same (mutable) `maxRealServed` across every call in a
// test to track the high-water mark over time.
static void checkNoReplay(const float* out, uint32_t frames, double pos, double& maxRealServed) {
    for (uint32_t i = 0; i < frames; i++) {
        if (out[i] == 0.f) continue;
        const double p = pos + i;
        assert(out[i] == static_cast<float>(p));
        assert(p >= maxRealServed);  // never replay an already-served position
        if (p + 1 > maxRealServed) maxRealServed = p + 1;
    }
}

// Fix (1), runtime re-review scenario (a): the app's write grid fills
// skipped slots with silence, so write_pos only ever *freezes* during a
// genuine stall (e.g. the app's thread briefly starved) - and while its
// heartbeat is still fresh (< 200ms), a sustained underrun against that
// frozen edge must NOT force a resync. Re-anchoring to w - readBehind while
// w is frozen would land the new anchor on audio already served earlier in
// this very test (since the reader was, until the stall, sitting exactly
// readBehind behind the edge) and replay it - a stutter. The reader should
// just ride out a growing, ordinary zero-filled underrun instead, resyncing
// only once the writer actually resumes or the heartbeat times out.
static void frozenWriteEdgeNoReplayTest() {
    const std::string name = "/rmtest.frozen." + std::to_string(getpid());
    SharedRegion r;
    assert(r.Create(name.c_str()));
    SharedLayout* l = r.layout();
    const uint64_t now = 16'000'000'000;
    const uint32_t frames = 512;

    writeMicIndexed(l, 4800, now);  // w = 4800; runway, and sets app_heartbeat_ns = now.
    float out[frames];
    double sampleTime = 1000;
    double maxRealServed = -1;

    r.ReadMic(sampleTime, out, frames, now);  // first sync
    const int64_t offset0 = r.DebugMicOffset();
    checkNoReplay(out, frames, sampleTime + offset0, maxRealServed);

    // The writer stalls: no more writes (write_pos frozen at 4800). Host
    // time still advances a little each cycle (as it would in practice),
    // but by only ~2ms/cycle, so after 9 cycles the stall is ~18ms old -
    // comfortably under the 200ms heartbeat timeout. By cycle 3-4 the
    // underrun would cross both sustained-underrun thresholds (3
    // consecutive cycles, and > kMicLatencyFrames accumulated shortfall) if
    // they weren't gated on the write edge actually advancing.
    for (int cycle = 0; cycle < 9; cycle++) {
        sampleTime += frames;
        const uint64_t cycleNow = now + static_cast<uint64_t>(cycle + 1) * 2'000'000;
        r.ReadMic(sampleTime, out, frames, cycleNow);
        assert(r.DebugMicOffset() == offset0);  // never re-anchors while w is frozen
        checkNoReplay(out, frames, sampleTime + offset0, maxRealServed);
    }

    r.Destroy();
}

// Fix (1), positive case: the gate added for scenario (a) must not disable
// the sustained-underrun mechanism entirely - it should still resync once a
// *genuinely* slow (as opposed to frozen) writer leaves the reader
// persistently underrunning against a live edge. Here the writer keeps
// write_pos advancing every cycle, just far more slowly (32 frames/cycle)
// than the reader is consuming (512 frames/cycle), so the shortfall grows
// each cycle until it crosses the accumulated-deficit threshold.
static void sustainedUnderrunWithAdvancingWriterResyncsTest() {
    const std::string name = "/rmtest.slow." + std::to_string(getpid());
    SharedRegion r;
    assert(r.Create(name.c_str()));
    SharedLayout* l = r.layout();
    const uint64_t now = 18'000'000'000;
    const uint32_t frames = 512;

    writeMicIndexed(l, 4800, now);  // w = 4800
    float out[frames];
    double sampleTime = 1000;
    double maxRealServed = -1;

    r.ReadMic(sampleTime, out, frames, now);  // first sync
    checkNoReplay(out, frames, sampleTime + r.DebugMicOffset(), maxRealServed);

    int resyncs = 0;
    int firstResyncCycle = -1;
    for (int cycle = 0; cycle < 8; cycle++) {
        writeMicIndexed(l, 32, now);  // write edge crawls forward - never frozen
        sampleTime += frames;
        const int64_t before = r.DebugMicOffset();
        r.ReadMic(sampleTime, out, frames, now);
        const int64_t after = r.DebugMicOffset();
        if (after != before) {
            resyncs++;
            if (firstResyncCycle < 0) firstResyncCycle = cycle;
        }
        checkNoReplay(out, frames, sampleTime + after, maxRealServed);
    }

    assert(resyncs >= 1);  // recovered rather than underrunning forever against a live edge
    assert(firstResyncCycle >= 0 && firstResyncCycle <= 5);  // within a handful of cycles

    r.Destroy();
}

// Fix (1)/(2), runtime re-review scenario (b): the "reader far ahead of the
// write edge" resync bound was loosened from a readBehind-scaled threshold
// to a fixed ~200ms (kMicAheadResyncFrames = 9600) window, since the app's
// write grid refills skipped slots with silence and catches write_pos back
// up on its own. A reader only modestly ahead of the edge - well beyond the
// old, tighter bound, but still under 9600 frames - must not resync.
static void readerBrieflyAheadNoResyncTest() {
    const std::string name = "/rmtest.ahead." + std::to_string(getpid());
    SharedRegion r;
    assert(r.Create(name.c_str()));
    SharedLayout* l = r.layout();
    const uint64_t now = 17'000'000'000;
    const uint32_t frames = 512;

    writeMic(l, 0.4f, 4800, now);  // w = 4800
    float out[frames];
    r.ReadMic(1000, out, frames, now);  // first sync: offset = (4800-1472)-1000 = 2328
    const int64_t offset0 = r.DebugMicOffset();
    for (float f : out) assert(f == 0.4f);

    // pos = 7000+2328 = 9328, i.e. 4528 frames ahead of the still-4800 write
    // edge - well beyond the old readBehind(+1440)-scaled bound, but under
    // the new 9600-frame one. Must not resync (and, since w hasn't moved,
    // wAdvancing is also false, so the sustained-underrun path can't fire
    // either, despite a single-cycle deficit far past kMicLatencyFrames).
    r.ReadMic(7000, out, frames, now);
    assert(r.DebugMicOffset() == offset0);

    // Sanity: comfortably past the 9600-frame bound, it still resyncs - the
    // bound was loosened, not removed. pos = 14000+2328 = 16328, 11528
    // frames ahead of the edge.
    r.ReadMic(14000, out, frames, now);
    assert(r.DebugMicOffset() != offset0);

    r.Destroy();
}

// Fix (2): the read-behind margin must be sized off the largest IO cycle
// any client on this device has ever requested (maxFramesSeen_), not just
// whichever client happens to call ReadMic - and resync - first. Two
// clients sharing one device (e.g. a 512-frame and a 1024-frame consumer)
// read with the same sampleTime; the smaller client is called first, which
// is exactly the ordering that could previously starve the larger client if
// the anchor were sized only from the first (smaller) call's own `frames`.
static void twoClientsNoStarvationTest() {
    const std::string name = "/rmtest.twoclient." + std::to_string(getpid());
    SharedRegion r;
    assert(r.Create(name.c_str()));
    SharedLayout* l = r.layout();
    const uint64_t now = 15'000'000'000;

    writeMicIndexed(l, 4800, now);  // w = 4800; runway for the first sync.

    float outSmall[512];
    float outBig[1024];
    double sampleTime = 2000;

    // Same sampleTime, smaller client (512) called first.
    r.ReadMic(sampleTime, outSmall, 512, now);
    r.ReadMic(sampleTime, outBig, 1024, now);
    const int64_t offset = r.DebugMicOffset();

    // Neither client underruns on this very first shared-sampleTime cycle:
    // the larger client is not starved just because the smaller client's
    // call came first and only knew about a 512-frame margin requirement at
    // the time it (re)synced.
    for (float f : outSmall) assert(f != 0.f);
    for (float f : outBig) assert(f != 0.f);

    // Keep the writer comfortably ahead (in step with the shared sampleTime
    // advance) for many more cycles and confirm neither client ever
    // underruns and the anchor never has to move again.
    for (int cycle = 0; cycle < 20; cycle++) {
        sampleTime += 1024;
        writeMicIndexed(l, 1024, now);
        r.ReadMic(sampleTime, outSmall, 512, now);
        r.ReadMic(sampleTime, outBig, 1024, now);
        assert(r.DebugMicOffset() == offset);  // no resync/starvation-driven thrash
        for (float f : outSmall) assert(f != 0.f);
        for (float f : outBig) assert(f != 0.f);
    }

    r.Destroy();
}

// Fix (2)/(5): models the app's real mic-write cadence (ticks roughly every
// 2ms + jitter, writing 480-frame slots while it's behind its own write
// schedule - see core/roommesh-core's runtime.rs, and the reviewer's
// scripts/sim/sim2.cpp harness this mirrors) against readers of every IO
// cycle size RoomMesh has to support, and asserts that once a reader has
// synced, it never sees a zero-filled sample again - i.e. that
// readBehind = max(kMicLatencyFrames, maxFramesSeen + kMicLatencyFrames)
// leaves enough margin for realistic write-timing jitter. This is the same
// property the reviewer's sweep validated (1440 simulated runs, 0
// zero-fill) that motivated sizing the margin this way.
static void sawtoothNoZeroFillAfterSyncTest(uint32_t frames, int jitterMaxFrames, uint64_t seed) {
    char nameBuf[80];
    std::snprintf(nameBuf, sizeof(nameBuf), "/rmtest.saw.%u.%d.%d", frames, jitterMaxFrames,
                  static_cast<int>(getpid()));
    SharedRegion r;
    assert(r.Create(nameBuf));
    SharedLayout* l = r.layout();
    const uint64_t hostNs0 = 20'000'000'000;

    std::mt19937_64 rng(seed);
    std::uniform_int_distribution<int> jit(0, jitterMaxFrames);

    int64_t nextTick = 0, nextOut = 0;
    bool started = false;
    int64_t nextRead = 5000;  // reader starts once the writer has a runway
    const int64_t T = 48000 * 5;  // 5 simulated seconds
    std::vector<float> out(frames);
    long realCyclesChecked = 0;

    auto hostNs = [&](int64_t t) { return hostNs0 + static_cast<uint64_t>(t * (1e9 / 48000.0)); };

    while (true) {
        const int64_t t = std::min(nextTick, nextRead);
        if (t > T) break;
        if (t == nextTick) {
            if (!started) { nextOut = t; started = true; }
            while (nextOut <= t + 480) {
                writeMic(l, 1.f, 480, hostNs(t));
                nextOut += 480;
            }
            nextTick = t + 96 + jit(rng);  // ~2ms nominal tick + 0..jitterMaxFrames of jitter
        } else {
            const double st = static_cast<double>(t - frames);
            r.ReadMic(st, out.data(), frames, hostNs(t));
            if (t > 20000) {  // past warmup/first-sync: steady state from here on
                for (float f : out) assert(f != 0.f);
                realCyclesChecked++;
            }
            nextRead = t + frames;
        }
    }
    assert(realCyclesChecked > 0);  // sanity: the loop actually exercised the steady state
    r.Destroy();
}

static void largeReaderStableNoResyncTest() {
    for (uint32_t frames : {256u, 480u, 512u, 1024u}) {
        for (int jitterFrames : {0, 96}) {  // 0ms and ~2ms of write-tick jitter
            sawtoothNoZeroFillAfterSyncTest(frames, jitterFrames, static_cast<uint64_t>(frames) * 1000 + jitterFrames);
        }
    }
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

    // app writes 100 ms of 0.5 → driver reads readBehind(512) = max(960, 512+960) = 1472
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
    frozenWriteEdgeNoReplayTest();
    sustainedUnderrunWithAdvancingWriterResyncsTest();
    readerBrieflyAheadNoResyncTest();
    twoClientsNoStarvationTest();
    largeReaderStableNoResyncTest();
    heartbeatZeroSilenceTest();

    std::puts("ring_test PASS");
    return 0;
}
