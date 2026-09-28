#include "Handlers.hpp"
#include <aspl/Stream.hpp>
#include <cassert>
#include <cstring>
#include <mutex>
#include <os/log.h>
#include <thread>

namespace roommesh {

void MicIOHandler::OnReadClientInput(const std::shared_ptr<aspl::Client>&, const std::shared_ptr<aspl::Stream>&,
                                     Float64, Float64 timestamp, void* bytes, UInt32 bytesCount) {
    region_->ReadMic(timestamp, static_cast<float*>(bytes), bytesCount / sizeof(float), HostNowNs());
}

void SpeakerIOHandler::OnWriteMixedOutput(const std::shared_ptr<aspl::Stream>& stream, Float64, Float64,
                                          const void* bytes, UInt32 bytesCount) {
    // Derive the channel count from the stream's actual format rather than
    // trusting the constructor argument, so a future format change to the
    // device can't silently desync the downmix from the real layout.
    const UInt32 channels = stream ? stream->GetChannelCount() : channels_;
    assert(channels == channels_ && "SpeakerIOHandler: stream channel count does not match expected value");
    region_->WriteSpeaker(static_cast<const float*>(bytes), bytesCount / (sizeof(float) * channels), channels, HostNowNs());
}

OSStatus IOStateHandler::OnStartIO() {
    if (auto* l = region_->layout()) (mic_ ? l->header.mic_clients : l->header.speaker_clients).store(1);
    // A fresh IO session should size ReadMic's read-behind margin off the
    // clients actually attached now, not a high-water mark left over from
    // a previous session that happened to have a larger-buffer client.
    if (mic_) region_->ResetIOStats();
    return kAudioHardwareNoError;
}
void IOStateHandler::OnStopIO() {
    if (auto* l = region_->layout()) (mic_ ? l->header.mic_clients : l->header.speaker_clients).store(0);
}

OSStatus DriverInit::OnInitialize() {
    // Warm mach_timebase_info's function-local static here (on the driver's
    // async-init thread) rather than letting it lazily initialize the first
    // time HostNowNs() is called from a realtime IO thread.
    HostNowNs();

    if (!region_->Create()) {
        os_log_error(OS_LOG_DEFAULT,
            "RoomMesh: failed to create shared memory %{public}s at step %{public}s (errno %d: %{public}s)",
            kShmName, region_->LastStep() ? region_->LastStep() : "?", region_->LastErrno(),
            std::strerror(region_->LastErrno()));
    } else {
        // Guard against launching more than one heartbeat thread if
        // OnInitialize() is ever invoked again (e.g. driver re-init).
        static std::once_flag heartbeatOnce;
        std::call_once(heartbeatOnce, [region = region_] {
            std::thread([region] {
                for (;;) { region->Heartbeat(HostNowNs()); std::this_thread::sleep_for(std::chrono::milliseconds(500)); }
            }).detach();
        });
    }
    return kAudioHardwareNoError;
}

}  // namespace roommesh
