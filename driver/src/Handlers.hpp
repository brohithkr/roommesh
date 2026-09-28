#pragma once
#include "SharedRegion.hpp"
#include <aspl/ControlRequestHandler.hpp>
#include <aspl/DriverRequestHandler.hpp>
#include <aspl/IORequestHandler.hpp>
#include <memory>

namespace roommesh {

class MicIOHandler : public aspl::IORequestHandler {
public:
    explicit MicIOHandler(std::shared_ptr<SharedRegion> r) : region_(std::move(r)) {}
    void OnReadClientInput(const std::shared_ptr<aspl::Client>& client, const std::shared_ptr<aspl::Stream>& stream,
                           Float64 zeroTimestamp, Float64 timestamp, void* bytes, UInt32 bytesCount) override;
private:
    std::shared_ptr<SharedRegion> region_;
};

class SpeakerIOHandler : public aspl::IORequestHandler {
public:
    // `channels` is the expected channel count (used only to sanity-check
    // against the stream's actual format at runtime); the channel count
    // actually used to interpret `bytes` is read from the stream itself via
    // Stream::GetChannelCount(), not hard-coded.
    SpeakerIOHandler(std::shared_ptr<SharedRegion> r, UInt32 channels) : region_(std::move(r)), channels_(channels) {}
    void OnWriteMixedOutput(const std::shared_ptr<aspl::Stream>& stream, Float64 zeroTimestamp, Float64 timestamp,
                            const void* bytes, UInt32 bytesCount) override;
private:
    std::shared_ptr<SharedRegion> region_;
    UInt32 channels_;
};

// Tracks whether IO is running on a device (published for diagnostics).
class IOStateHandler : public aspl::ControlRequestHandler {
public:
    IOStateHandler(std::shared_ptr<SharedRegion> r, bool mic) : region_(std::move(r)), mic_(mic) {}
    OSStatus OnStartIO() override;
    void OnStopIO() override;
private:
    std::shared_ptr<SharedRegion> region_;
    bool mic_;
};

// Creates the shared memory once the HAL has initialised the plug-in.
class DriverInit : public aspl::DriverRequestHandler {
public:
    explicit DriverInit(std::shared_ptr<SharedRegion> r) : region_(std::move(r)) {}
    OSStatus OnInitialize() override;
private:
    std::shared_ptr<SharedRegion> region_;
};

}  // namespace roommesh
