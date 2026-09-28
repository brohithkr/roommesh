#include "Devices.hpp"
#include <aspl/Stream.hpp>
#include <string>

namespace roommesh {

std::shared_ptr<aspl::Device> MakeDevice(const std::shared_ptr<aspl::Context>& ctx, const char* name, const char* uid,
                                          UInt32 channels, aspl::Direction dir) {
    aspl::DeviceParameters p;
    p.Name = name;
    p.Manufacturer = "RoomMesh";
    p.DeviceUID = uid;
    p.ModelUID = std::string(uid) + "_Model";
    p.SampleRate = kSampleRate;
    p.ChannelCount = channels;
    p.EnableMixing = true;
    p.CanBeDefault = true;
    p.CanBeDefaultForSystemSounds = false;
    auto d = std::make_shared<aspl::Device>(ctx, p);

    // Explicit Float32 format: AddStreamWithControlsAsync(Direction) alone
    // would keep libASPL's default StreamParameters::Format (44.1 kHz,
    // 16-bit signed int), but OnReadClientInput/OnWriteMixedOutput below
    // treat the buffer as native-format Float32 samples (bytesCount =
    // frames * mBytesPerFrame). Field order matches AudioStreamBasicDescription
    // declaration order to avoid -Wreorder-init-list.
    aspl::StreamParameters sp;
    sp.Direction = dir;
    sp.Format = {
        .mSampleRate = kSampleRate,
        .mFormatID = kAudioFormatLinearPCM,
        .mFormatFlags = kAudioFormatFlagIsFloat | kAudioFormatFlagsNativeEndian | kAudioFormatFlagIsPacked,
        .mBytesPerPacket = 4 * channels,
        .mFramesPerPacket = 1,
        .mBytesPerFrame = 4 * channels,
        .mChannelsPerFrame = channels,
        .mBitsPerChannel = 32,
    };
    d->AddStreamWithControlsAsync(sp);
    return d;
}

}  // namespace roommesh
