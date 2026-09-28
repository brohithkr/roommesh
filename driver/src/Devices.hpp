#pragma once
#include "SharedLayout.hpp"
#include <aspl/Context.hpp>
#include <aspl/Device.hpp>
#include <memory>

namespace roommesh {

// Builds an aspl::Device with an explicit 48 kHz native-endian interleaved
// Float32 stream format. libASPL's StreamParameters::Format defaults to
// 44100 Hz / 16-bit signed integer; RoomMesh's SharedRegion::ReadMic /
// WriteSpeaker and the IO handlers operate on Float32 samples end-to-end,
// so the stream format must be set explicitly rather than left at the
// libASPL default. Factored out of Entry.cpp so device_format_test can
// exercise the exact same construction path.
std::shared_ptr<aspl::Device> MakeDevice(const std::shared_ptr<aspl::Context>& ctx, const char* name, const char* uid,
                                          UInt32 channels, aspl::Direction dir);

}  // namespace roommesh
