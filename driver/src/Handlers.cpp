#include "Handlers.hpp"
#include <os/log.h>
#include <thread>

namespace roommesh {

void MicIOHandler::OnReadClientInput(const std::shared_ptr<aspl::Client>&, const std::shared_ptr<aspl::Stream>&,
                                     Float64, Float64 timestamp, void* bytes, UInt32 bytesCount) {
    region_->ReadMic(timestamp, static_cast<float*>(bytes), bytesCount / sizeof(float), HostNowNs());
}

void SpeakerIOHandler::OnWriteMixedOutput(const std::shared_ptr<aspl::Stream>&, Float64, Float64,
                                          const void* bytes, UInt32 bytesCount) {
    region_->WriteSpeaker(static_cast<const float*>(bytes), bytesCount / (sizeof(float) * channels_), channels_, HostNowNs());
}

OSStatus IOStateHandler::OnStartIO() {
    if (auto* l = region_->layout()) (mic_ ? l->header.mic_clients : l->header.speaker_clients).store(1);
    return kAudioHardwareNoError;
}
void IOStateHandler::OnStopIO() {
    if (auto* l = region_->layout()) (mic_ ? l->header.mic_clients : l->header.speaker_clients).store(0);
}

OSStatus DriverInit::OnInitialize() {
    if (!region_->Create()) {
        os_log_error(OS_LOG_DEFAULT, "RoomMesh: failed to create shared memory %{public}s (errno %d)", kShmName, errno);
    } else {
        auto region = region_;
        std::thread([region] {
            for (;;) { region->Heartbeat(HostNowNs()); std::this_thread::sleep_for(std::chrono::milliseconds(500)); }
        }).detach();
    }
    return kAudioHardwareNoError;
}

}  // namespace roommesh
