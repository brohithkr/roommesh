#include "SharedRegion.hpp"
#include <algorithm>
#include <cerrno>
#include <cstring>
#include <fcntl.h>
#include <mach/mach_time.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

namespace roommesh {

namespace {

// micState_ packs (synced, offset) into one 64-bit word: bit 63 is the
// synced flag, the remaining 63 bits hold `offset` (sign-extended). A
// device sample-time-derived offset would need centuries of continuous
// 48kHz runtime to need more than 62 bits plus a sign bit, so dropping the
// top bit is never observed in practice.
constexpr uint64_t kMicStateSyncedBit = 1ull << 63;

uint64_t PackMicState(bool synced, int64_t offset) {
    const uint64_t bits = static_cast<uint64_t>(offset) & ~kMicStateSyncedBit;
    return (synced ? kMicStateSyncedBit : 0ull) | bits;
}
bool UnpackMicSynced(uint64_t state) { return (state & kMicStateSyncedBit) != 0; }
int64_t UnpackMicOffset(uint64_t state) {
    uint64_t bits = state & ~kMicStateSyncedBit;
    if (bits & (1ull << 62)) bits |= kMicStateSyncedBit;  // sign-extend bit 62 -> bit 63
    return static_cast<int64_t>(bits);
}

// Relaxed atomic max via CAS retry: used for the cross-client high-water
// marks (maxFramesSeen_, servedRealEnd_) ReadMic maintains. These are
// heuristics/bookkeeping, not synchronization, so relaxed ordering suffices.
template <typename T>
void AtomicMaxRelaxed(std::atomic<T>& a, T v) {
    T cur = a.load(std::memory_order_relaxed);
    while (v > cur && !a.compare_exchange_weak(cur, v, std::memory_order_relaxed)) {}
}

// "Reader far ahead of the write edge" resync threshold. Loosened (from an
// earlier `readBehind`-scaled bound) to a fixed ~200ms window: the app's
// write grid fills skipped slots with silence and catches write_pos back up
// on its own, and the 200ms heartbeat gate above already handles the app
// actually dying - so a reader only modestly ahead of a live writer should
// just ride out a short underrun (see the sustained-underrun handling
// below) rather than resync onto a fresh anchor. 200ms @ 48kHz = 9600
// frames.
constexpr int64_t kMicAheadResyncFrames = 9600;

}  // namespace

uint64_t HostNowNs() {
    static mach_timebase_info_data_t tb = [] { mach_timebase_info_data_t t; mach_timebase_info(&t); return t; }();
    return static_cast<uint64_t>((static_cast<__uint128_t>(mach_absolute_time()) * tb.numer) / tb.denom);
}

SharedRegion::~SharedRegion() {
    if (layout_) munmap(layout_, sizeof(SharedLayout));
}

bool SharedRegion::Create(const char* name) {
    name_ = name;
    lastErrno_ = 0;
    lastStep_ = nullptr;

    // Drop a stale region left behind by a previous coreaudiod process (or a
    // previous test run). Not itself a failure: shm_unlink() legitimately
    // returns ENOENT when there is nothing to remove, and even if it fails
    // for another reason, the O_CREAT|O_EXCL open below is the real
    // source of truth and will surface any remaining problem.
    shm_unlink(name);

    // Decision 4: the shm region is mode 0666 so any local process (the app,
    // running as the logged-in user, and the driver, running inside
    // coreaudiod) can attach to it without a privileged daemon. fchmod() on
    // a POSIX shm object returns EINVAL on macOS, so the only way to get
    // that mode is to clear umask for the O_CREAT call itself. This does
    // mean the region is readable/writable by any local user/process, which
    // is accepted per Decision 4 (no secrets flow through it - only PCM).
    mode_t old = umask(0);
    // O_EXCL (after the shm_unlink above) guarantees we always create a
    // fresh, zero-filled region rather than reusing one that might have a
    // stale generation/layout from a differently-versioned driver.
    int fd = shm_open(name, O_CREAT | O_EXCL | O_RDWR, 0666);
    umask(old);
    if (fd < 0) {
        lastErrno_ = errno;
        lastStep_ = "shm_open";
        return false;
    }
    if (ftruncate(fd, sizeof(SharedLayout)) != 0) {
        lastErrno_ = errno;
        lastStep_ = "ftruncate";
        close(fd);
        return false;
    }
    void* p = mmap(nullptr, sizeof(SharedLayout), PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    close(fd);
    if (p == MAP_FAILED) {
        lastErrno_ = errno;
        lastStep_ = "mmap";
        return false;
    }
    layout_ = static_cast<SharedLayout*>(p);
    std::memset(static_cast<void*>(layout_), 0, sizeof(SharedLayout));
    layout_->header.magic = kMagic;
    layout_->header.version = kVersion;
    layout_->header.sample_rate = kSampleRate;
    layout_->header.ring_frames = kRingFrames;
    layout_->header.generation.store(HostNowNs(), std::memory_order_release);
    return true;
}

void SharedRegion::Destroy() {
    if (layout_) { munmap(layout_, sizeof(SharedLayout)); layout_ = nullptr; }
    if (!name_.empty()) shm_unlink(name_.c_str());
}

void SharedRegion::ReadMic(double sampleTime, float* out, uint32_t frames, uint64_t nowNs) {
    if (!layout_) { std::fill(out, out + frames, 0.f); return; }
    Ring& ring = layout_->mic;
    const uint64_t w = ring.h.write_pos.load(std::memory_order_acquire);
    const uint64_t beat = layout_->header.app_heartbeat_ns.load(std::memory_order_relaxed);

    // Whether the write edge moved since the last ReadMic call on this
    // region (from any client). Used below to gate the sustained-underrun
    // resync: re-anchoring while write_pos is frozen (the app stalled
    // briefly but its heartbeat is still fresh) would land the new anchor
    // on stale, already-served audio and replay it (a stutter) - a frozen
    // writer should just be ridden out as a growing, ordinary zero-filled
    // underrun (or caught by the heartbeat gate once it's been long
    // enough), never re-anchored onto.
    const uint64_t prevW = lastMicW_.exchange(w, std::memory_order_relaxed);
    const bool wAdvancing = w > prevW;

    // The minimum distance to serve behind the write edge scales with the
    // largest IO cycle size any client has ever requested on this region
    // (maxFramesSeen_), not just this call's own `frames`: every client
    // shares one anchor (micState_), so sizing the margin off whichever
    // client happens to call - and resync - first would starve a larger
    // client that calls afterward with the same or a stale sampleTime.
    // kMicLatencyFrames is both the floor and the extra headroom added on
    // top of maxFramesSeen_; that sizing came from simulating the app's
    // real write cadence (~2ms ticks + timing jitter): 0 zero-filled
    // cycles across 1440 simulated runs. Note: nowNs > beat + 200ms is
    // also how the app signalling app_heartbeat_ns = 0 (it stopped being
    // the room's coordinator) produces immediate silence below - beat == 0
    // always satisfies that inequality.
    AtomicMaxRelaxed(maxFramesSeen_, frames);
    const uint64_t readBehind = std::max<uint64_t>(
        kMicLatencyFrames, static_cast<uint64_t>(maxFramesSeen_.load(std::memory_order_relaxed)) + kMicLatencyFrames);

    if (nowNs > beat + 200'000'000ull || w < readBehind + frames) {
        std::fill(out, out + frames, 0.f);
        micState_.fetch_and(~kMicStateSyncedBit, std::memory_order_relaxed);
        underrunStreak_.store(0, std::memory_order_relaxed);
        underrunDeficit_.store(0, std::memory_order_relaxed);
        return;
    }

    const int64_t st = static_cast<int64_t>(sampleTime);
    const uint64_t state = micState_.load(std::memory_order_relaxed);
    const bool synced = UnpackMicSynced(state);
    int64_t off = UnpackMicOffset(state);
    int64_t pos = st + off;

    // Resync (re-anchor to the write edge) when: this is the first read;
    // the mapping has drifted far out of range in either direction (the
    // two conditions below); or the reader has been stuck underrunning for
    // several consecutive cycles running (see "sustained underrun" below -
    // a writer that has slowed or stalled just slightly, never far enough
    // to trip either out-of-range bound on its own). Never resync merely
    // because this one cycle underruns in isolation (pos + frames > w):
    // re-anchoring backward on a one-off underrun would replay
    // already-served audio and produce an audible buzz. A brief underrun
    // is instead handled below by serving what exists and zero-filling the
    // rest, keeping the anchor stable so a steady app with small jitter
    // never glitches.
    //
    // The two out-of-range conditions bound latency in both directions:
    //  - pos far ahead of w (more than ~200ms/kMicAheadResyncFrames past the
    //    edge) means the previous mapping no longer makes sense, e.g. the
    //    app (re)started and reset its write position. This bound is
    //    deliberately loose (not readBehind-scaled): the app's write grid
    //    refills skipped slots with silence and catches w back up on its
    //    own, so a reader only modestly ahead of a live writer should ride
    //    out a short underrun (below) rather than resync.
    //  - w far ahead of pos (more than readBehind + 1440 frames) means a
    //    burst of writes - e.g. queued audio flushed right after the app
    //    stalled - has pushed the edge far beyond where this reader is
    //    tracking. Left alone this would permanently add latency, so
    //    resync back near the edge instead of letting the gap persist.
    bool resync = !synced ||
                  pos > static_cast<int64_t>(w) + kMicAheadResyncFrames ||
                  static_cast<int64_t>(w) - pos > static_cast<int64_t>(readBehind + 1440);

    // Sustained underrun: the two bounds above only fire once the gap
    // between pos and w is already large. A writer that has only slowed
    // down (not stalled) - never triggering either bound, but persistently
    // a little short of a full cycle's worth of frames because the reader
    // is sitting too close to a *live* edge - would otherwise underrun
    // forever without ever resyncing, silently adding latency cycle after
    // cycle. Track consecutive underrun cycles and the accumulated
    // shortfall; force a resync once either crosses a small threshold
    // (3 cycles, or ~20ms accumulated) so the anchor can't get stuck - but
    // only while write_pos is actually advancing (wAdvancing). If the
    // writer is simply frozen (a brief stall, heartbeat still fresh),
    // re-anchoring would land on stale, already-served audio and replay it
    // (a stutter); a frozen writer is instead left to ride out as a
    // growing zero-filled underrun until either it resumes (at which point
    // the already-primed streak/deficit counters resync almost
    // immediately) or the heartbeat gate above eventually silences it.
    const bool wouldUnderrun = !resync && pos + static_cast<int64_t>(frames) > static_cast<int64_t>(w);
    if (wouldUnderrun) {
        const uint32_t deficit =
            static_cast<uint32_t>(pos + static_cast<int64_t>(frames) - static_cast<int64_t>(w));
        const uint32_t streak = underrunStreak_.fetch_add(1, std::memory_order_relaxed) + 1;
        const uint32_t accDeficit = underrunDeficit_.fetch_add(deficit, std::memory_order_relaxed) + deficit;
        if (wAdvancing && (streak >= 3 || accDeficit > kMicLatencyFrames)) resync = true;
    } else if (!resync) {
        underrunStreak_.store(0, std::memory_order_relaxed);
        underrunDeficit_.store(0, std::memory_order_relaxed);
    }

    if (resync) {
        off = static_cast<int64_t>(w - readBehind) - st;
        micState_.store(PackMicState(true, off), std::memory_order_relaxed);
        pos = st + off;
        underrunStreak_.store(0, std::memory_order_relaxed);
        underrunDeficit_.store(0, std::memory_order_relaxed);
    }

    // pos is unconstrained in general (it's exactly w - readBehind, i.e.
    // strictly < w, right after a resync; otherwise it's whatever this
    // call's own sampleTime maps to under the existing, unchanged offset).
    // Under a *stable* offset, upos only ever moves forward as sampleTime
    // advances, even across different clients sharing that offset at the
    // same or overlapping sampleTime (by design: "every client in the same
    // cycle ... receives the same audio", so two clients legitimately
    // reading the same range at the same instant is not a replay, even
    // though one of them may compute a smaller upos than the other already
    // advanced servedRealEnd_ to). A resync changing the offset is the only
    // thing that can make upos jump backward in absolute terms relative to
    // what's already been delivered - so only while the *current* offset
    // differs from servedRealOffset_ (the offset in effect when
    // servedRealEnd_ was last advanced, i.e. we haven't yet made real
    // forward progress under this offset) do we guard against reading ring
    // memory below servedRealEnd_, the highest absolute position ever
    // served as real (non-zero-filled) audio: re-serving that range would
    // replay old audio instead of leaving a one-off silent gap. This
    // condition self-clears (and stays clear until the next resync) as
    // soon as one call under the new offset reaches real, unmasked data.
    // Never read at or past `w` either way, since nothing has been written
    // there yet.
    const uint64_t upos = static_cast<uint64_t>(pos);
    uint64_t realStart = upos;
    uint32_t skip = 0;
    if (off != servedRealOffset_.load(std::memory_order_relaxed)) {
        const uint64_t servedEnd = servedRealEnd_.load(std::memory_order_relaxed);
        realStart = std::max(upos, servedEnd);
        skip = realStart > upos ? static_cast<uint32_t>(std::min<uint64_t>(frames, realStart - upos)) : 0;
    }
    const uint32_t avail =
        realStart < w ? static_cast<uint32_t>(std::min<uint64_t>(frames - skip, w - realStart)) : 0;
    for (uint32_t i = 0; i < skip; i++) out[i] = 0.f;
    for (uint32_t i = 0; i < avail; i++) out[skip + i] = ring.samples[(realStart + i) & kRingMask];
    for (uint32_t i = skip + avail; i < frames; i++) out[i] = 0.f;
    if (avail > 0) {
        AtomicMaxRelaxed(servedRealEnd_, realStart + avail);
        servedRealOffset_.store(off, std::memory_order_relaxed);
    }

    // read_pos is a diagnostic/latency high-water mark ("served through
    // this absolute position"), not a guarantee that everything in
    // [old read_pos, new read_pos) was real audio: on an underrun cycle,
    // `end` (and hence the published read_pos) can land past the current
    // write_pos, since the zero-filled tail still counts as "served" for
    // bookkeeping purposes.
    const uint64_t end = upos + frames;
    if (end > ring.h.read_pos.load(std::memory_order_relaxed)) {
        ring.h.read_pos.store(end, std::memory_order_release);
        ring.h.read_host_ns.store(nowNs, std::memory_order_relaxed);
    }
}

int64_t SharedRegion::DebugMicOffset() const {
    return UnpackMicOffset(micState_.load(std::memory_order_relaxed));
}

void SharedRegion::ResetIOStats() {
    maxFramesSeen_.store(0, std::memory_order_relaxed);
}

void SharedRegion::WriteSpeaker(const float* in, uint32_t frames, uint32_t channels, uint64_t nowNs) {
    if (!layout_ || channels == 0) return;
    Ring& ring = layout_->speaker;
    const uint64_t w = ring.h.write_pos.load(std::memory_order_relaxed);
    for (uint32_t f = 0; f < frames; f++) {
        float s = 0.f;
        for (uint32_t c = 0; c < channels; c++) s += in[f * channels + c];
        ring.samples[(w + f) & kRingMask] = s / static_cast<float>(channels);
    }
    ring.h.write_host_ns.store(nowNs, std::memory_order_relaxed);
    ring.h.write_pos.store(w + frames, std::memory_order_release);
}

void SharedRegion::Heartbeat(uint64_t nowNs) {
    if (layout_) layout_->header.driver_heartbeat_ns.store(nowNs, std::memory_order_relaxed);
}

}  // namespace roommesh
