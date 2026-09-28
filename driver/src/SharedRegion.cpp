#include "SharedRegion.hpp"
#include <algorithm>
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
    shm_unlink(name);                 // drop a stale region from a previous coreaudiod
    mode_t old = umask(0);            // fchmod on shm objects is EINVAL on macOS: set mode at creation
    int fd = shm_open(name, O_CREAT | O_RDWR, 0666);
    umask(old);
    if (fd < 0) return false;
    if (ftruncate(fd, sizeof(SharedLayout)) != 0) { close(fd); return false; }
    void* p = mmap(nullptr, sizeof(SharedLayout), PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    close(fd);
    if (p == MAP_FAILED) return false;
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
    if (nowNs > beat + 200'000'000ull || w < kMicLatencyFrames + frames) {
        std::fill(out, out + frames, 0.f);
        micSynced_.store(false, std::memory_order_relaxed);
        return;
    }
    const int64_t st = static_cast<int64_t>(sampleTime);
    int64_t off = micOffset_.load(std::memory_order_relaxed);
    int64_t pos = st + off;
    const bool resync = !micSynced_.load(std::memory_order_relaxed) ||
                        pos + static_cast<int64_t>(frames) > static_cast<int64_t>(w) ||
                        static_cast<int64_t>(w) - pos > static_cast<int64_t>(kRingFrames - 4800);
    if (resync) {
        off = static_cast<int64_t>(w - kMicLatencyFrames) - st;
        micOffset_.store(off, std::memory_order_relaxed);
        micSynced_.store(true, std::memory_order_relaxed);
        pos = st + off;
    }
    for (uint32_t i = 0; i < frames; i++) out[i] = ring.samples[(static_cast<uint64_t>(pos) + i) & kRingMask];
    const uint64_t end = static_cast<uint64_t>(pos) + frames;
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
