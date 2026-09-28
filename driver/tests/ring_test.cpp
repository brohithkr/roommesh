#include "SharedRegion.hpp"
#include <algorithm>
#include <atomic>
#include <cassert>
#include <chrono>
#include <cstdio>
#include <random>
#include <string>
#include <thread>
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
    // (offset0's pos). Fix (2)'s per-client replay guard must recognize that
    // range as already-delivered (to this same client, clientId 0 by
    // default) and zero-fill it rather than replay it - re-serving the same
    // 512 samples a second time would be exactly the kind of buzz this
    // guard exists to prevent.
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
// Each client uses its own clientId (mirroring MicIOHandler, which passes
// aspl::Client::GetClientID()), exercising the per-client replay guard too -
// with a shared/default clientId, the smaller client's own progress would
// incorrectly mask the larger client's reads of the very same range.
static void twoClientsNoStarvationTest(uint32_t smallFrames, uint32_t bigFrames) {
    const std::string name =
        "/rm2c." + std::to_string(smallFrames) + "." + std::to_string(bigFrames) + "." + std::to_string(getpid());
    SharedRegion r;
    assert(r.Create(name.c_str()));
    SharedLayout* l = r.layout();
    const uint64_t now = 15'000'000'000;
    const uint32_t kSmallClientId = 101, kBigClientId = 202;

    writeMicIndexed(l, 4800, now);  // w = 4800; runway for the first sync.

    std::vector<float> outSmall(smallFrames), outBig(bigFrames);
    double sampleTime = 2000;

    // Same sampleTime, smaller client called first.
    r.ReadMic(sampleTime, outSmall.data(), smallFrames, now, kSmallClientId);
    r.ReadMic(sampleTime, outBig.data(), bigFrames, now, kBigClientId);
    const int64_t offset = r.DebugMicOffset();

    // Neither client underruns on this very first shared-sampleTime cycle:
    // the larger client is not starved just because the smaller client's
    // call came first and only knew about its own (smaller) margin
    // requirement at the time it (re)synced.
    for (float f : outSmall) assert(f != 0.f);
    for (float f : outBig) assert(f != 0.f);

    // Keep the writer comfortably ahead (in step with the shared sampleTime
    // advance) for many more cycles and confirm neither client ever
    // underruns and the anchor never has to move again.
    for (int cycle = 0; cycle < 20; cycle++) {
        sampleTime += bigFrames;
        writeMicIndexed(l, bigFrames, now);
        r.ReadMic(sampleTime, outSmall.data(), smallFrames, now, kSmallClientId);
        r.ReadMic(sampleTime, outBig.data(), bigFrames, now, kBigClientId);
        assert(r.DebugMicOffset() == offset);  // no resync/starvation-driven thrash
        for (float f : outSmall) assert(f != 0.f);
        for (float f : outBig) assert(f != 0.f);
    }

    r.Destroy();
}

// Fix (4)/regression guard: with frame sizes close together (512 vs 1024),
// even a buggy per-call readBehind (computed from each call's own `frames`
// rather than the shared maxFramesSeen_ high-water mark) can accidentally
// still cover the larger client, since the resync formula (off = w -
// readBehind - st) happens to leave exactly `readBehind` frames of margin -
// which was >= 1024 anyway. Frame sizes that differ by more than
// kMicLatencyFrames (960) expose the bug for what it is: a per-call margin
// sized for the smaller client's first call is *permanently* too shallow
// for the larger one, not just on the first, transitional cycle.
static void twoClientsNoStarvationWideGapTest() {
    const std::string name = "/rmtest.twoclientwide." + std::to_string(getpid());
    SharedRegion r;
    assert(r.Create(name.c_str()));
    SharedLayout* l = r.layout();
    const uint64_t now = 15'500'000'000;
    const uint32_t kSmallFrames = 256, kBigFrames = 2048;
    const uint32_t kSmallClientId = 111, kBigClientId = 222;

    writeMic(l, 1.f, 4800, now);  // w = 4800; runway for the first sync.

    std::vector<float> outSmall(kSmallFrames), outBig(kBigFrames);
    double sampleTime = 2000;

    r.ReadMic(sampleTime, outSmall.data(), kSmallFrames, now, kSmallClientId);
    r.ReadMic(sampleTime, outBig.data(), kBigFrames, now, kBigClientId);

    // The very first shared cycle can legitimately underrun the larger
    // client: its size isn't known to maxFramesSeen_ until its own first
    // call, one call *after* the anchor was already set from the smaller
    // client's call. What must not happen is *persistent* starvation -
    // maxFramesSeen_ has to make the *next* resync (triggered by the
    // resulting underrun) deep enough for both clients from then on.
    for (int cycle = 0; cycle < 10; cycle++) {
        sampleTime += kBigFrames;
        writeMic(l, 1.f, kBigFrames, now);
        r.ReadMic(sampleTime, outSmall.data(), kSmallFrames, now, kSmallClientId);
        r.ReadMic(sampleTime, outBig.data(), kBigFrames, now, kBigClientId);
    }

    // By now the anchor must have settled deep enough for the larger
    // client, and stay settled: several more cycles with zero underrun for
    // either client, and no further resyncs (a per-call-margin regression
    // instead zero-fills the larger client on *every* cycle, forever, since
    // its own margin requirement never gets remembered).
    const int64_t offset = r.DebugMicOffset();
    for (int cycle = 0; cycle < 10; cycle++) {
        sampleTime += kBigFrames;
        writeMic(l, 1.f, kBigFrames, now);
        r.ReadMic(sampleTime, outSmall.data(), kSmallFrames, now, kSmallClientId);
        r.ReadMic(sampleTime, outBig.data(), kBigFrames, now, kBigClientId);
        assert(r.DebugMicOffset() == offset);
        for (float f : outSmall) assert(f != 0.f);
        for (float f : outBig) assert(f != 0.f);
    }

    r.Destroy();
}

// Fix (2)/(5): models the app's real mic-write cadence (ticks roughly every
// 2ms + jitter, writing 480-frame slots while it's behind its own write
// schedule - see core/roommesh-core's runtime.rs) against readers of every
// IO cycle size RoomMesh has to support, and asserts that once a reader has
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

// Fix (1) (hand-back stale replay): after the app's heartbeat drops to 0 or
// goes stale (a coordinator hand-off, or the app dying and a new one taking
// over), the *next* resync must never serve data written before that gap -
// even though that data was never technically "already served" to this
// reader, so the per-client replay guard (fix 2) alone wouldn't catch it;
// this needs the separate, session-scoped silenceFloor_ set in ReadMic's
// silence branch. Model a session hand-off with session-tagged audio:
// session 1 tags its writes 1.0, then a heartbeat gap, then session 2
// (write_pos simply continuing from wherever session 1 left off, exactly
// like the real shared ring) tags its writes 2.0. The reader must never see
// 1.0 again once the gap starts - only 2.0 or silence.
static void handoffNeverReplaysPreviousSessionTest() {
    for (uint32_t frames : {256u, 512u, 1024u}) {
        const std::string name = "/rmtest.handoff." + std::to_string(frames) + "." + std::to_string(getpid());
        SharedRegion r;
        assert(r.Create(name.c_str()));
        SharedLayout* l = r.layout();
        const uint64_t now = 21'000'000'000;
        std::vector<float> out(frames);
        double sampleTime = 1000;

        // Session 1: steady, synced reading of audio tagged 1.0.
        writeMic(l, 1.f, 4800, now);
        r.ReadMic(sampleTime, out.data(), frames, now);
        for (float f : out) assert(f == 1.f);
        for (int cycle = 0; cycle < 20; cycle++) {
            writeMic(l, 1.f, frames, now);
            sampleTime += frames;
            r.ReadMic(sampleTime, out.data(), frames, now);
            for (float f : out) assert(f == 1.f);
        }

        // Hand-off: heartbeat drops to 0 (session 1 relinquishes coordinator,
        // or dies outright). write_pos is frozen - nothing more is written
        // for session 1.
        l->header.app_heartbeat_ns.store(0);
        for (int cycle = 0; cycle < 20; cycle++) {
            sampleTime += frames;
            r.ReadMic(sampleTime, out.data(), frames, now);
            for (float f : out) assert(f == 0.f);  // silence, heartbeat stale
        }

        // Session 2 resumes (write_pos continues forward from wherever
        // session 1 left off), tagged 2.0. From here on, every sample must
        // be 2.0 or silence - never the stale 1.0 tag from before the gap.
        bool sawFresh = false;
        for (int cycle = 0; cycle < 40; cycle++) {
            writeMic(l, 2.f, frames, now);
            sampleTime += frames;
            r.ReadMic(sampleTime, out.data(), frames, now);
            for (float f : out) {
                assert(f == 2.f || f == 0.f);  // never 1.f (fix 1)
                if (f == 2.f) sawFresh = true;
            }
        }
        assert(sawFresh);  // sanity: real session-2 audio does eventually flow

        r.Destroy();
    }
}

// Fix (2): a quick StopIO/StartIO - the HAL's sample-time domain restarting
// at (or near) 0 while ReadMic's offset would otherwise be left untouched -
// must never let the reader compute a position it already served before the
// restart. Two belts: ResetIOStats() now also clears the synced flag, so
// the anchor doesn't rely on a sampleTime domain that no longer applies;
// and even if it didn't, the per-client replay guard independently
// guarantees no replay either way. Indexed values (each sample tagged with
// its own absolute position) make any repeat directly visible.
static void quickRestartNoReplayTest() {
    for (int cyclesBeforeRestart : {1, 2, 4, 8}) {
        const std::string name =
            "/rmtest.restart." + std::to_string(cyclesBeforeRestart) + "." + std::to_string(getpid());
        SharedRegion r;
        assert(r.Create(name.c_str()));
        SharedLayout* l = r.layout();
        const uint64_t now = 22'000'000'000;
        const uint32_t frames = 512;

        writeMicIndexed(l, 4800, now);
        float out[frames];
        double sampleTime = 0;
        double maxServed = -1;
        auto checkAndTrack = [&](double pos) {
            for (uint32_t i = 0; i < frames; i++) {
                if (out[i] == 0.f) continue;
                const double p = pos + i;
                assert(out[i] == static_cast<float>(p));
                assert(p >= maxServed);  // never replay an already-served position
                if (p + 1 > maxServed) maxServed = p + 1;
            }
        };

        for (int c = 0; c < cyclesBeforeRestart; c++) {
            r.ReadMic(sampleTime, out, frames, now);
            checkAndTrack(sampleTime + r.DebugMicOffset());
            sampleTime += frames;
            writeMicIndexed(l, frames, now);
        }

        // StopIO/StartIO: a little more data trickles in (as it would while
        // the device is briefly reconfigured), IO stats reset, and the
        // HAL's sample time restarts at 0 for the new IO session.
        writeMicIndexed(l, 240, now);
        const int64_t offsetBeforeRestart = r.DebugMicOffset();
        r.ResetIOStats();
        sampleTime = 0;

        // ResetIOStats() must force a fresh resync immediately, rather than
        // leaving the reader to fall back on the per-client replay guard to
        // silently zero-fill cycles until its old high-water mark is caught
        // back up to: the offset must already differ on this very first
        // post-restart call, and that call must serve real audio (not the
        // guard papering over a stale anchor with silence).
        r.ReadMic(sampleTime, out, frames, now);
        assert(r.DebugMicOffset() != offsetBeforeRestart);
        checkAndTrack(sampleTime + r.DebugMicOffset());
        long realFirstCycle = 0;
        for (float f : out) if (f != 0.f) realFirstCycle++;
        assert(realFirstCycle > 0);
        sampleTime += frames;
        writeMicIndexed(l, frames, now);

        for (int c = 0; c < 5; c++) {
            r.ReadMic(sampleTime, out, frames, now);
            checkAndTrack(sampleTime + r.DebugMicOffset());
            sampleTime += frames;
            writeMicIndexed(l, frames, now);
        }

        r.Destroy();
    }
}

// Fix (2), end-aligned multi-client scenario: two clients with different
// frame sizes read independently (each ending its own IO cycle at its own
// device time, not synchronized to the other) while the writer goes through
// a slow patch that triggers a resync. Before the per-client replay guard,
// the *first* client to get real data after a backward-re-anchoring resync
// would clear a single shared "guard is off" flag, letting the *second*
// client replay - even though the second client's own read position hadn't
// itself caught up past what it had already been served.
static void endAlignedMultiClientNoReplayTest() {
    for (bool bFirst : {false, true}) {
        const std::string name = "/rmtest.endaligned." + std::to_string(bFirst) + "." + std::to_string(getpid());
        SharedRegion r;
        assert(r.Create(name.c_str()));
        SharedLayout* l = r.layout();
        const uint64_t now = 23'000'000'000;
        const uint32_t FA = 256, FB = 512;
        const uint32_t kClientA = 11, kClientB = 22;

        writeMicIndexed(l, 6000, now);

        float a[FA], b[FB];
        double maxA = -1, maxB = -1;
        auto rd = [&](float* o, uint32_t F, uint32_t clientId, double st, double& mx) {
            r.ReadMic(st, o, F, now, clientId);
            const double pos = st + r.DebugMicOffset();
            for (uint32_t i = 0; i < F; i++) {
                if (o[i] == 0.f) continue;
                const double p = pos + i;
                assert(o[i] == static_cast<float>(p));
                assert(p >= mx);  // never replay an already-served position
                if (p + 1 > mx) mx = p + 1;
            }
        };

        int64_t t = 6000;
        for (int cyc = 0; cyc < 400; cyc++) {
            t += FB;
            // Writer: normal, then a slow patch (underruns -> resync), then normal again.
            const uint32_t n = (cyc >= 100 && cyc < 130) ? 400u : FB;
            writeMicIndexed(l, n, now);
            // Both clients end-aligned on the same IO cycle boundary (B is
            // driven once per cycle; A, being half B's size, is driven
            // twice per B cycle).
            if (bFirst) {
                rd(b, FB, kClientB, static_cast<double>(t - FB), maxB);
                rd(a, FA, kClientA, static_cast<double>(t - FA), maxA);
            } else {
                rd(a, FA, kClientA, static_cast<double>(t - FA), maxA);
                rd(b, FB, kClientB, static_cast<double>(t - FB), maxB);
            }
            rd(a, FA, kClientA, static_cast<double>(t - FB), maxA);  // A's other half-cycle (earlier)
        }

        r.Destroy();
    }
}

// Fix (1) (client slots never released): before ReleaseClientSlot, a
// per-client table slot claimed by a HAL client that then disconnects was
// never freed - after kMaxTrackedMicClients (16) distinct clients over
// coreaudiod's lifetime, the guard "fails open" (FindOrCreateClientSlot
// returns nullptr) for every new client from then on, silently disabling
// the replay guard for all of them. Simulate more than 16 clients
// connecting, reading once, and disconnecting in sequence - mirroring what
// IOStateHandler::OnRemoveClient now does via ReleaseClientSlot - then a
// brand new client hits a large backward sampleTime jump (a resync landing
// back on already-served audio). It must still be fully guarded: zero
// replayed samples, regardless of how many clients came and went before it.
static void clientSlotsReleasedNoLeakTest() {
    for (int priorClients : {15, 16, 20}) {
        const std::string name = "/rmslots." + std::to_string(priorClients) + "." + std::to_string(getpid());
        SharedRegion r;
        assert(r.Create(name.c_str()));
        SharedLayout* l = r.layout();
        const uint64_t now = 25'000'000'000;

        writeMicIndexed(l, 6000, now);
        float out[512];

        // `priorClients` HAL clients come and go, one at a time.
        for (int id = 100; id < 100 + priorClients; id++) {
            r.ReadMic(1000, out, 512, now, static_cast<uint32_t>(id));
            r.ReleaseClientSlot(static_cast<uint32_t>(id));
        }

        // A brand new client (never seen before) reads steadily...
        const uint32_t me = 999;
        double maxServed = -1;
        double sampleTime = 2000;
        auto checkAndTrack = [&](double pos) {
            for (uint32_t i = 0; i < 512; i++) {
                if (out[i] == 0.f) continue;
                const double p = pos + i;
                assert(out[i] == static_cast<float>(p));
                assert(p >= maxServed);  // never replay an already-served position
                if (p + 1 > maxServed) maxServed = p + 1;
            }
        };
        for (int c = 0; c < 4; c++) {
            writeMicIndexed(l, 512, now);
            r.ReadMic(sampleTime, out, 512, now, me);
            checkAndTrack(sampleTime + r.DebugMicOffset());
            sampleTime += 512;
        }
        // ...then takes a large backward sampleTime jump, forcing a resync
        // that lands back on a range it already read. With slots properly
        // released, `me` gets its own clean slot regardless of how many
        // clients came before it, and the guard must mask that overlap:
        // zero replay.
        r.ReadMic(-1'000'000, out, 512, now, me);
        checkAndTrack(-1'000'000 + r.DebugMicOffset());

        r.Destroy();
    }
}

// Fix (3): a resync can legitimately land the anchor deeper than the
// current steady-state readBehind - e.g. against a write edge that had
// briefly raced ahead without quite crossing the "burst" resync bound
// (readBehind + 1440), or one computed while maxFramesSeen_ was inflated by
// a client that's since gone. If that excess margin *persists* (not just a
// one-off), the anchor should be trimmed forward, once, back to the normal
// depth after about half a second of simulated host time - the skipped
// positions were never served to anyone, so this isn't a replay.
//
// Re-review: the trim must compare like with like. `gap` here is an
// *end*-gap (write edge minus the end of what a cycle serves,
// w - (pos + frames)); readBehind is a *start*-gap (w - pos right at a
// fresh resync). The steady-state end-gap right after any resync is
// normalEndGap = readBehind - frames, which - since readBehind is always
// exactly frames + kMicLatencyFrames for a single client - works out to a
// frame-size-independent kMicLatencyFrames (960) here. Comparing `gap`
// against readBehind + 480 (mixing the two scales) would be off by
// `frames`, so for a 1024-frame reader the effective slack more than
// doubles and the trim almost never fires; comparing against
// normalEndGap + 480 is the intended, scale-consistent threshold.
static void anchorDepthForwardTrimTest(uint32_t frames, int64_t extraJump) {
    const std::string name =
        "/rmtrim." + std::to_string(frames) + "." + std::to_string(extraJump) + "." + std::to_string(getpid());
    SharedRegion r;
    assert(r.Create(name.c_str()));
    SharedLayout* l = r.layout();
    uint64_t now = 26'000'000'000;
    const uint64_t nsPerCycle = static_cast<uint64_t>(frames) * (1'000'000'000ull / 48000);

    writeMic(l, 1.f, 4800, now);
    std::vector<float> out(frames);
    double sampleTime = 1000;

    r.ReadMic(sampleTime, out.data(), frames, now);  // first sync: end-gap settles at normalEndGap
    for (float f : out) assert(f == 1.f);
    const int64_t offset0 = r.DebugMicOffset();

    // Inflate the end-gap by `extraJump` frames in one shot - comfortably
    // under the "burst" resync bound (start-gap > readBehind + 1440) but,
    // for extraJump in {900, 1200}, above normalEndGap (960) + 480 (1440).
    writeMic(l, 1.f, frames + static_cast<uint32_t>(extraJump), now);
    sampleTime += frames;
    r.ReadMic(sampleTime, out.data(), frames, now);
    for (float f : out) assert(f == 1.f);
    assert(r.DebugMicOffset() == offset0);  // confirms the bump alone didn't resync

    // Hold that excess steady for a bit over 700ms of simulated host time,
    // with a small (deterministic, zero-mean every 2 cycles) sawtooth
    // jitter on the writer's per-cycle amount - the trim must survive
    // ordinary write-timing jitter rather than needing a perfectly flat
    // gap to ever fire.
    const int cycles = static_cast<int>((700'000'000ull + nsPerCycle - 1) / nsPerCycle);
    int resyncs = 0;
    int64_t lastOffset = offset0;
    for (int cycle = 0; cycle < cycles; cycle++) {
        const int32_t jitter = (cycle % 2 == 0) ? 48 : -48;
        writeMic(l, 1.f, static_cast<uint32_t>(static_cast<int64_t>(frames) + jitter), now);
        sampleTime += frames;
        now += nsPerCycle;
        r.ReadMic(sampleTime, out.data(), frames, now);
        for (float f : out) assert(f == 1.f);  // never underruns while/after trimming
        const int64_t offset = r.DebugMicOffset();
        if (offset != lastOffset) resyncs++;
        lastOffset = offset;
    }

    // The anchor must have been trimmed forward - exactly once, not
    // repeatedly thrashing - back to (near) the normal depth, not left
    // sitting on the extraJump-frame surplus forever.
    assert(resyncs == 1);
    const int64_t readBehind = static_cast<int64_t>(readBehindFor(frames));
    const int64_t normalEndGap = readBehind - static_cast<int64_t>(frames);
    const uint64_t w = l->mic.h.write_pos.load();
    const double posAfter = sampleTime + static_cast<double>(lastOffset);
    const int64_t gapAfter = static_cast<int64_t>(w) - static_cast<int64_t>(posAfter) - static_cast<int64_t>(frames);
    assert(gapAfter >= 0 && gapAfter <= normalEndGap + 480);

    r.Destroy();
}

static void anchorDepthForwardTrimSuiteTest() {
    for (uint32_t frames : {512u, 1024u}) {
        for (int64_t extraJump : {900, 1200}) {
            anchorDepthForwardTrimTest(frames, extraJump);
        }
    }
}

// Speaker-ring seqlock (RingHeader::seq): WriteSpeaker must leave seq even
// after every call (never observably odd from outside the call, since
// WriteSpeaker has a single caller and returns only once the final
// even store has happened), incrementing by exactly 2 per write, and
// distinct from write_pos/write_host_ns's own values - a regression that
// forgot to advance seq, or advanced it by the wrong amount, would silently
// break the Rust reader's ability to detect an in-progress write.
static void speakerSeqlockTest() {
    const std::string name = "/rmtest.seq." + std::to_string(getpid());
    SharedRegion r;
    assert(r.Create(name.c_str()));
    SharedLayout* l = r.layout();
    assert(l->speaker.h.seq.load() == 0);  // matches the Rust reader's "old driver" sentinel

    float mono[4] = {0.1f, 0.2f, 0.3f, 0.4f};
    uint64_t expectedSeq = 0;
    for (uint64_t i = 1; i <= 5; i++) {
        r.WriteSpeaker(mono, 4, 1, 1'000'000'000 * i);
        expectedSeq += 2;
        const uint64_t seq = l->speaker.h.seq.load();
        assert(seq == expectedSeq);
        assert(seq % 2 == 0);  // never left odd
        assert(l->speaker.h.write_pos.load() == 4 * i);
        assert(l->speaker.h.write_host_ns.load() == 1'000'000'000 * i);
    }

    // The mic ring shares RingHeader but the driver never touches seq
    // there - it must stay at 0 regardless of mic activity.
    writeMic(l, 1.f, 512, 6'000'000'000);
    assert(l->mic.h.seq.load() == 0);

    r.Destroy();
}

// Two-thread stress test for the speaker-ring seqlock (RingHeader::seq):
// a writer thread calls the real WriteSpeaker in a loop with `nowNs`
// derived from the write position (nowNs = 1000*(w+frames), so a
// consistent (write_pos, write_host_ns) pair always satisfies
// write_host_ns == 1000*write_pos); this thread concurrently runs the same
// acquire/fence/recheck protocol as the Rust reader
// (SpeakerReader::write_state in virtual_device.rs) and asserts every pair
// it accepts (didn't exhaust its retries) is internally consistent - never
// torn. Runs for ~200ms, which is enough for many thousands of writes and
// reads to interleave on real hardware. Verified to fail (an inconsistent
// pair gets accepted) if WriteSpeaker's odd-marker store is removed.
static void speakerSeqlockStressTest() {
    const std::string name = "/rmtest.seqstress." + std::to_string(getpid());
    SharedRegion r;
    assert(r.Create(name.c_str()));
    SharedLayout* l = r.layout();
    RingHeader& h = l->speaker.h;

    std::atomic<bool> stop{false};
    float buf[2 * 7] = {0};
    std::thread writer([&] {
        uint64_t w = 0;
        while (!stop.load(std::memory_order_relaxed)) {
            r.WriteSpeaker(buf, 7, 2, 1000 * (w + 7));
            w += 7;
        }
    });

    long accepted = 0;
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::milliseconds(200);
    while (std::chrono::steady_clock::now() < deadline) {
        uint64_t w = 0, t = 0;
        bool got = false;
        for (int attempt = 0; attempt < 8; attempt++) {
            const uint64_t s1 = h.seq.load(std::memory_order_acquire);
            if (s1 == 0) break;   // no write yet; nothing to check this iteration
            if (s1 & 1) continue;  // odd: a write is in progress right now
            w = h.write_pos.load(std::memory_order_relaxed);
            t = h.write_host_ns.load(std::memory_order_relaxed);
            std::atomic_thread_fence(std::memory_order_acquire);
            const uint64_t s2 = h.seq.load(std::memory_order_relaxed);
            if (s1 == s2) { got = true; break; }
        }
        if (!got) continue;  // exhausted retries, or nothing written yet: skip this sample
        assert(t == 1000 * w);  // every accepted pair must be internally consistent, never torn
        accepted++;
    }

    stop.store(true, std::memory_order_relaxed);
    writer.join();
    assert(accepted > 0);  // sanity: the loop actually exercised the seqlock, not a no-op

    r.Destroy();
}

// Fix: Create() must reset every mic-side tracking field, including
// silenceFloor_ - a SharedRegion instance reused across more than one shm
// region (e.g. a driver re-init calling Create() again) must not carry
// stale bookkeeping from a previous session into a brand new region whose
// write_pos starts back at 0. Drives silenceFloor_ up through a real
// session (write a lot of audio, then let the heartbeat go stale so the
// silence branch marks everything written so far as off-limits), re-
// Creates the same SharedRegion object, and confirms fresh audio at the
// new region's much smaller write positions is actually served rather than
// being masked forever by the old session's now-meaningless floor.
static void createTwiceResetsTrackingStateTest() {
    const std::string name = "/rmtest.create2x." + std::to_string(getpid());
    SharedRegion r;
    assert(r.Create(name.c_str()));
    SharedLayout* l = r.layout();

    const uint64_t now1 = 10'000'000'000;
    writeMic(l, 1.f, 20000, now1);  // w = 20000
    float out[512];
    r.ReadMic(1000, out, 512, now1);  // establishes a real anchor
    for (float f : out) assert(f == 1.f);
    r.ReadMic(2024, out, 512, now1 + 500'000'000);  // heartbeat now stale -> silence branch,
    for (float f : out) assert(f == 0.f);           // silenceFloor_ marked up to 20000

    // Re-Create the *same* SharedRegion object: a brand new shm region
    // (same name - Create() unlinks any existing region at that name
    // first), whose write_pos starts back at 0.
    assert(r.Create(name.c_str()));
    SharedLayout* l2 = r.layout();
    assert(l2->mic.h.write_pos.load() == 0);

    // Fresh audio at a small write position. Without resetting
    // silenceFloor_ (left over at >= 20000 from the previous session), the
    // new region's entire live range would sit below that stale floor and
    // be masked to silence forever.
    const uint64_t now2 = 20'000'000'000;
    writeMic(l2, 2.f, 4800, now2);
    r.ReadMic(1000, out, 512, now2);
    for (float f : out) assert(f == 2.f);

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
    assert(l->speaker.h.seq.load() == 0);  // fresh region: no write yet, matches "old driver" sentinel
    float st[8] = {1, 0, 0.5f, 0.5f, 0, 0, -1, 1};
    r.WriteSpeaker(st, 4, 2, now);
    assert(l->speaker.h.write_pos.load() == 4);
    assert(l->speaker.samples[0] == 0.5f && l->speaker.samples[1] == 0.5f && l->speaker.samples[3] == 0.f);
    assert(l->speaker.h.write_host_ns.load() == now);
    assert(l->speaker.h.seq.load() == 2);  // even, incremented by 2, never left odd

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
    twoClientsNoStarvationTest(512, 1024);
    twoClientsNoStarvationWideGapTest();
    handoffNeverReplaysPreviousSessionTest();
    quickRestartNoReplayTest();
    endAlignedMultiClientNoReplayTest();
    clientSlotsReleasedNoLeakTest();
    anchorDepthForwardTrimSuiteTest();
    largeReaderStableNoResyncTest();
    speakerSeqlockTest();
    speakerSeqlockStressTest();
    createTwiceResetsTrackingStateTest();
    heartbeatZeroSilenceTest();

    std::puts("ring_test PASS");
    return 0;
}
