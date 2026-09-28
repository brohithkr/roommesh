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

// Relaxed atomic max/min via CAS retry: used for the various high-water
// (and low-water) marks ReadMic maintains. These are heuristics/bookkeeping,
// not synchronization, so relaxed ordering suffices.
template <typename T>
void AtomicMaxRelaxed(std::atomic<T>& a, T v) {
    T cur = a.load(std::memory_order_relaxed);
    while (v > cur && !a.compare_exchange_weak(cur, v, std::memory_order_relaxed)) {}
}
template <typename T>
void AtomicMinRelaxed(std::atomic<T>& a, T v) {
    T cur = a.load(std::memory_order_relaxed);
    while (v < cur && !a.compare_exchange_weak(cur, v, std::memory_order_relaxed)) {}
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
    // A SharedRegion instance that's reused across more than one shm region
    // (e.g. a driver re-init calling Create() again) must not carry any
    // mic-side bookkeeping - an offset, a client's high-water mark, an
    // in-progress trim window - into this brand new region, whose
    // write_pos starts back at 0 and has no relationship to whatever this
    // object last saw.
    ResetMicTrackingState();
    return true;
}

void SharedRegion::Destroy() {
    if (layout_) { munmap(layout_, sizeof(SharedLayout)); layout_ = nullptr; }
    if (!name_.empty()) shm_unlink(name_.c_str());
}

SharedRegion::ClientServedEnd* SharedRegion::FindOrCreateClientSlot(uint32_t clientId, uint64_t nowNs) {
    // First pass: this client may already have a slot.
    for (auto& slot : micClientServed_) {
        if (slot.clientId.load(std::memory_order_relaxed) == clientId) {
            slot.lastSeenNs.store(nowNs, std::memory_order_relaxed);
            return &slot;
        }
    }
    // Second pass: claim a free slot. A plain linear scan + CAS is fine
    // here - this only runs when a new client shows up (rare relative to
    // the steady-state ReadMic call rate), and kMaxTrackedMicClients is
    // small. Reset servedEnd so a slot a new client claims can never
    // observe a stale high-water mark left over from nothing (a freshly
    // constructed/reset slot already has servedEnd 0, but this also covers
    // ReleaseClientSlot leaving a slot's servedEnd non-zero if it's ever
    // reordered relative to clearing clientId - see ReleaseClientSlot).
    for (auto& slot : micClientServed_) {
        uint32_t expected = kNoClient;
        if (slot.clientId.compare_exchange_strong(expected, clientId, std::memory_order_relaxed)) {
            slot.servedEnd.store(0, std::memory_order_relaxed);
            slot.lastSeenNs.store(nowNs, std::memory_order_relaxed);
            return &slot;
        }
    }
    // Fallback: every slot is claimed, but one may belong to a client that
    // disconnected through a path that never reached ReleaseClientSlot
    // (see IOStateHandler::OnRemoveClient). A slot idle for over
    // kClientSlotIdleNs is eligible to be reclaimed by a new client -
    // this is a heuristic safety net, not the primary release mechanism.
    for (auto& slot : micClientServed_) {
        const uint64_t seen = slot.lastSeenNs.load(std::memory_order_relaxed);
        if (nowNs <= seen + kClientSlotIdleNs) continue;
        uint32_t expected = slot.clientId.load(std::memory_order_relaxed);
        if (expected == kNoClient) continue;  // someone else's release/claim race; try the next slot
        if (slot.clientId.compare_exchange_strong(expected, clientId, std::memory_order_relaxed)) {
            slot.servedEnd.store(0, std::memory_order_relaxed);
            slot.lastSeenNs.store(nowNs, std::memory_order_relaxed);
            return &slot;
        }
    }
    return nullptr;  // table full of still-active clients; this client's reads simply aren't guarded (see header comment)
}

void SharedRegion::ReleaseClientSlot(uint32_t clientId) {
    for (auto& slot : micClientServed_) {
        if (slot.clientId.load(std::memory_order_relaxed) == clientId) {
            // Clear servedEnd before the clientId, so a concurrent
            // FindOrCreateClientSlot that's about to claim this now-freed
            // slot can never observe the old clientId's high-water mark.
            slot.servedEnd.store(0, std::memory_order_relaxed);
            slot.clientId.store(kNoClient, std::memory_order_relaxed);
            return;
        }
    }
}

void SharedRegion::ResetMicTrackingState() {
    micState_.store(0, std::memory_order_relaxed);
    maxFramesSeen_.store(0, std::memory_order_relaxed);
    underrunStreak_.store(0, std::memory_order_relaxed);
    underrunDeficit_.store(0, std::memory_order_relaxed);
    lastMicW_.store(0, std::memory_order_relaxed);
    silenceFloor_.store(0, std::memory_order_relaxed);
    marginWindowStartNs_.store(0, std::memory_order_relaxed);
    marginWindowMinGap_.store(0, std::memory_order_relaxed);
    for (auto& slot : micClientServed_) {
        slot.servedEnd.store(0, std::memory_order_relaxed);
        slot.lastSeenNs.store(0, std::memory_order_relaxed);
        slot.clientId.store(kNoClient, std::memory_order_relaxed);
    }
}

void SharedRegion::ReadMic(double sampleTime, float* out, uint32_t frames, uint64_t nowNs, uint32_t clientId) {
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
        // Mark everything written so far as off-limits to any future real
        // serve (see silenceFloor_): a stale/zero heartbeat means the app's
        // session may be ending here (a coordinator hand-off, or a new app
        // process entirely). When it resumes and the next resync re-anchors
        // to (the by-then-larger) w - readBehind, that position must never
        // land on this pre-gap tail and serve it as if it were fresh -
        // whether or not *this* reader personally already saw it.
        AtomicMaxRelaxed(silenceFloor_, w);
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
    // Guard against ever serving stale/already-delivered content as real
    // audio: never read below this *client's own* high-water mark (a
    // resync that re-anchors backward relative to what this specific
    // client already received would otherwise replay it - but a different
    // client that simply hasn't reached as far yet must not be silenced by
    // that; every client in the same cycle legitimately reading the same
    // range at the same instant is not a replay), and never read below
    // silenceFloor_ either (audio written before an app_heartbeat_ns gap,
    // which must never resurface after the gap even to a client that never
    // personally saw it - see the silence branch above). Never read at or
    // past `w`, since nothing has been written there yet.
    const uint64_t upos = static_cast<uint64_t>(pos);
    ClientServedEnd* clientSlot = FindOrCreateClientSlot(clientId, nowNs);
    const uint64_t clientServedEnd = clientSlot ? clientSlot->servedEnd.load(std::memory_order_relaxed) : 0;
    const uint64_t floor = std::max(clientServedEnd, silenceFloor_.load(std::memory_order_relaxed));
    const uint64_t realStart = std::max(upos, floor);
    const uint32_t skip = realStart > upos ? static_cast<uint32_t>(std::min<uint64_t>(frames, realStart - upos)) : 0;
    const uint32_t avail =
        realStart < w ? static_cast<uint32_t>(std::min<uint64_t>(frames - skip, w - realStart)) : 0;
    for (uint32_t i = 0; i < skip; i++) out[i] = 0.f;
    for (uint32_t i = 0; i < avail; i++) out[skip + i] = ring.samples[(realStart + i) & kRingMask];
    for (uint32_t i = skip + avail; i < frames; i++) out[i] = 0.f;
    if (avail > 0 && clientSlot) AtomicMaxRelaxed(clientSlot->servedEnd, realStart + avail);

    // Fix: anchor depth. A resync can legitimately land deeper than the
    // current steady-state readBehind - e.g. it was computed while
    // maxFramesSeen_ (and hence readBehind) was inflated by a larger
    // client that has since disconnected, or against a write edge that had
    // temporarily surged ahead. If the margin between the write edge and
    // what's actually being served (the gap) stays *persistently* above
    // the normal steady-state end-gap plus 480 - not just a one-off - for
    // about half a second, trim the anchor forward, once, back to the
    // normal depth. The positions being skipped over were never served to
    // anyone (the gap was never small enough to reach them), so this is
    // not a replay, just shedding latency the anchor no longer needs to
    // carry.
    //
    // `gap` and `normalEndGap` are both *end*-gaps (write edge minus the
    // end of what this cycle serves, i.e. w - (upos + frames)) - readBehind
    // itself is a *start*-gap (w - upos at the instant of a fresh resync).
    // Comparing gap directly against readBehind + 480 mixes the two scales
    // and is off by `frames`: for a 1024-frame reader that's over double
    // the intended slack, so the trim would almost never fire. normalEndGap
    // (readBehind - frames) is what `gap` actually settles to in steady
    // state right after a resync, so comparing against normalEndGap + 480
    // measures the intended ~480-frame surplus, on the same scale, for any
    // frame size.
    const int64_t gap = static_cast<int64_t>(w) - static_cast<int64_t>(upos + frames);
    const int64_t normalEndGap = static_cast<int64_t>(readBehind) - static_cast<int64_t>(frames);
    const int64_t trimThreshold = normalEndGap + 480;
    if (!resync && gap > trimThreshold) {
        const uint64_t windowStart = marginWindowStartNs_.load(std::memory_order_relaxed);
        if (windowStart == 0) {
            marginWindowStartNs_.store(nowNs, std::memory_order_relaxed);
            marginWindowMinGap_.store(static_cast<uint64_t>(gap), std::memory_order_relaxed);
        } else {
            AtomicMinRelaxed(marginWindowMinGap_, static_cast<uint64_t>(gap));
            if (nowNs - windowStart >= 500'000'000ull) {
                const uint64_t minGap = marginWindowMinGap_.load(std::memory_order_relaxed);
                if (static_cast<int64_t>(minGap) > trimThreshold) {
                    // Shift the offset forward by exactly the sustained
                    // surplus (minGap - normalEndGap), landing the new
                    // end-gap at normalEndGap - the normal depth - rather
                    // than recomputing from w - readBehind (which would
                    // re-derive a start-gap and reintroduce the same
                    // start/end mismatch this fix corrects).
                    const int64_t trimmedOff = off + (static_cast<int64_t>(minGap) - normalEndGap);
                    micState_.store(PackMicState(true, trimmedOff), std::memory_order_relaxed);
                    underrunStreak_.store(0, std::memory_order_relaxed);
                    underrunDeficit_.store(0, std::memory_order_relaxed);
                }
                marginWindowStartNs_.store(0, std::memory_order_relaxed);
            }
        }
    } else {
        marginWindowStartNs_.store(0, std::memory_order_relaxed);
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
    // Force a fresh resync on the next ReadMic call rather than reusing a
    // stale offset: a StopIO/StartIO typically resets the HAL's sample-time
    // domain to 0, and an unchanged offset against that would otherwise map
    // to a position this session already served (the per-client replay
    // guard in ReadMic independently prevents that from ever coming out as
    // audio, but skips straight to a fresh anchor here rather than paying
    // for the guard to silently mask frames until the old high-water mark
    // is caught back up to).
    micState_.fetch_and(~kMicStateSyncedBit, std::memory_order_relaxed);
    underrunStreak_.store(0, std::memory_order_relaxed);
    underrunDeficit_.store(0, std::memory_order_relaxed);
    lastMicW_.store(0, std::memory_order_relaxed);
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
    // Seqlock (see RingHeader::seq in SharedLayout.hpp): mark odd before
    // touching write_host_ns/write_pos, fence so that marking is ordered
    // before those stores, publish both, then mark even. WriteSpeaker has
    // exactly one caller (this device's IO thread), so `s` needs no atomic
    // read-modify-write - nothing else can be concurrently advancing it.
    const uint64_t s = ring.h.seq.load(std::memory_order_relaxed);
    ring.h.seq.store(s + 1, std::memory_order_relaxed);
    std::atomic_thread_fence(std::memory_order_release);
    ring.h.write_host_ns.store(nowNs, std::memory_order_relaxed);
    ring.h.write_pos.store(w + frames, std::memory_order_release);
    ring.h.seq.store(s + 2, std::memory_order_release);
}

void SharedRegion::Heartbeat(uint64_t nowNs) {
    if (layout_) layout_->header.driver_heartbeat_ns.store(nowNs, std::memory_order_relaxed);
}

}  // namespace roommesh
