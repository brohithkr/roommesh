// Regression test for the CRITICAL review finding on Task 33: MakeDevice()
// must set an explicit Float32/48kHz stream format. Without it,
// AddStreamWithControlsAsync(Direction) keeps libASPL's default
// StreamParameters::Format (44.1kHz, 16-bit signed int), while
// SharedRegion::ReadMic/WriteSpeaker and the IO handlers treat the HAL's
// buffer as native-format Float32 samples.
#include "Devices.hpp"
#include <aspl/Context.hpp>
#include <aspl/Stream.hpp>
#include <cassert>
#include <cstdio>

using namespace roommesh;

static void checkFormat(const std::shared_ptr<aspl::Device>& device, aspl::Direction dir, UInt32 channels) {
    auto stream = device->GetStreamByIndex(dir, 0);
    assert(stream != nullptr);

    const AudioStreamBasicDescription fmt = stream->GetPhysicalFormat();
    assert(fmt.mSampleRate == kSampleRate);
    assert(fmt.mFormatID == kAudioFormatLinearPCM);
    assert((fmt.mFormatFlags & kAudioFormatFlagIsFloat) != 0);
    assert((fmt.mFormatFlags & kAudioFormatFlagIsSignedInteger) == 0);
    assert(fmt.mBitsPerChannel == 32);
    assert(fmt.mChannelsPerFrame == channels);
    assert(fmt.mBytesPerFrame == 4 * channels);
    assert(fmt.mBytesPerPacket == 4 * channels);
    assert(fmt.mFramesPerPacket == 1);

    assert(stream->GetChannelCount() == channels);
}

int main() {
    auto ctx = std::make_shared<aspl::Context>();

    auto mic = MakeDevice(ctx, "RoomMesh Microphone", "RoomMeshMicrophone_UID", 1, aspl::Direction::Input);
    checkFormat(mic, aspl::Direction::Input, /*channels=*/1);   // 4 bytes/frame

    auto spk = MakeDevice(ctx, "RoomMesh Speaker", "RoomMeshSpeaker_UID", 2, aspl::Direction::Output);
    checkFormat(spk, aspl::Direction::Output, /*channels=*/2);  // 8 bytes/frame

    std::puts("device_format_test PASS");
    return 0;
}
