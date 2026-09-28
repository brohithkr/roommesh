#include "Devices.hpp"
#include "Handlers.hpp"
#include <aspl/Driver.hpp>
#include <CoreAudio/AudioServerPlugIn.h>

using namespace roommesh;

namespace {

std::shared_ptr<aspl::Driver> CreateDriver() {
    auto ctx = std::make_shared<aspl::Context>();
    auto region = std::make_shared<SharedRegion>();

    auto mic = MakeDevice(ctx, "RoomMesh Microphone", "RoomMeshMicrophone_UID", 1, aspl::Direction::Input);
    mic->SetIOHandler(std::make_shared<MicIOHandler>(region));
    mic->SetControlHandler(std::make_shared<IOStateHandler>(region, true));

    auto spk = MakeDevice(ctx, "RoomMesh Speaker", "RoomMeshSpeaker_UID", 2, aspl::Direction::Output);
    spk->SetIOHandler(std::make_shared<SpeakerIOHandler>(region, 2));
    spk->SetControlHandler(std::make_shared<IOStateHandler>(region, false));

    auto plugin = std::make_shared<aspl::Plugin>(ctx);
    plugin->AddDevice(mic);
    plugin->AddDevice(spk);

    auto driver = std::make_shared<aspl::Driver>(ctx, plugin);
    driver->SetDriverHandler(std::make_shared<DriverInit>(region));
    return driver;
}

}  // namespace

// The bundle is built with -fvisibility=hidden; this is the only symbol the
// HAL loads (via CFPlugInFactories in Info.plist), so export it explicitly.
extern "C" __attribute__((visibility("default"))) void* RoomMeshEntryPoint(CFAllocatorRef, CFUUIDRef typeUUID) {
    if (!CFEqual(typeUUID, kAudioServerPlugInTypeUUID)) return nullptr;
    static std::shared_ptr<aspl::Driver> driver = CreateDriver();
    return driver->GetReference();
}
