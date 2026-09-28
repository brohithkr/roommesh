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

    // The minimum distance to serve behind the write edge scales with this
    // caller's own IO cycle size, so a client with a larger buffer (e.g. a
    // 1024-frame aggregate device) isn't forced into a resync every cycle
    // just because kMicLatencyFrames (960, sized for the ~10 ms write slot
    // cadence) is close to its buffer size. kMicLatencyFrames remains the
    // floor for small buffers. Note: nowNs > beat + 200ms is also how the
    // app signalling app_heartbeat_ns = 0 (it stopped being the room's
    // coordinator) produces immediate silence below - beat == 0 always
    // satisfies that inequality.
    const uint64_t readBehind = std::max<uint64_t>(kMicLatencyFrames, static_cast<uint64_t>(frames) + 480);

    if (nowNs > beat + 200'000'000ull || w < readBehind + frames) {
        std::fill(out, out + frames, 0.f);
        micSynced_.store(false, std::memory_order_relaxed);
        return;
    }

    const int64_t st = static_cast<int64_t>(sampleTime);
    int64_t off = micOffset_.load(std::memory_order_relaxed);
    int64_t pos = st + off;

    // Resync (re-anchor to the write edge) only when the app has clearly
    // (re)started or the sampleTime->ring-position mapping has drifted far
    // out of range - never merely because this cycle would underrun
    // (pos + frames > w). Re-anchoring backward on a plain underrun would
    // replay already-served audio and produce an audible buzz; that case is
    // instead handled below by serving what actually exists and
    // zero-filling the rest, keeping the anchor stable so a steady app with
    // small jitter never glitches.
    //
    // The two out-of-range conditions bound latency in both directions:
    //  - pos far ahead of w (more than a full readBehind past the edge)
    //    means the previous mapping no longer makes sense, e.g. the app
    //    (re)started and reset its write position.
    //  - w far ahead of pos (more than readBehind + 1440 frames) means a
    //    burst of writes - e.g. queued audio flushed right after the app
    //    stalled - has pushed the edge far beyond where this reader is
    //    tracking. Left alone this would permanently add latency, so
    //    resync back near the edge instead of letting the gap persist.
    const bool resync = !micSynced_.load(std::memory_order_relaxed) ||
                        pos > static_cast<int64_t>(w + readBehind) ||
                        static_cast<int64_t>(w) - pos > static_cast<int64_t>(readBehind + 1440);
    if (resync) {
        off = static_cast<int64_t>(w - readBehind) - st;
        micOffset_.store(off, std::memory_order_relaxed);
        micSynced_.store(true, std::memory_order_relaxed);
        pos = st + off;
    }

    // pos is now within [0, w]: the gate above guarantees w >= readBehind +
    // frames, so a fresh resync (pos = w - readBehind) lands in [0, w]; and
    // when we didn't resync, the two conditions just checked being false
    // means pos already satisfies 0 <= pos <= w. Serve whatever has
    // actually been written and zero-fill any unwritten tail (a plain
    // underrun) rather than reading past the write edge.
    const uint64_t upos = static_cast<uint64_t>(pos);
    const uint32_t avail = upos < w ? static_cast<uint32_t>(std::min<uint64_t>(frames, w - upos)) : 0;
    for (uint32_t i = 0; i < avail; i++) out[i] = ring.samples[(upos + i) & kRingMask];
    for (uint32_t i = avail; i < frames; i++) out[i] = 0.f;

    const uint64_t end = upos + frames;
    if (end > ring.h.read_pos.load(std::memory_order_relaxed)) {
        ring.h.read_pos.store(end, std::memory_order_release);
        ring.h.read_host_ns.store(nowNs, std::memory_order_relaxed);
    }
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
