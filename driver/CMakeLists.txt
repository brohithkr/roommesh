cmake_minimum_required(VERSION 3.20)
# NOTE (toolchain adaptation, root cause): on this machine, when only
# CommandLineTools was selected (no full Xcode), its default "MacOSX.sdk"
# symlink resolved to a mismatched MacOSX27.0.sdk, whose .tbd stub
# libraries advertised an architecture triple ("arm64e.x1-macos") that
# that CommandLineTools' own linker/tapi (ld-1267 / tapi-2100.0.2.6) could
# not parse ("tapi error: malformed file ... unknown architecture") -
# breaking linking for any arch, not just x86_64. Now that full Xcode
# 26.5 is selected (`xcode-select -p` -> .../Xcode.app/Contents/Developer),
# `xcrun --sdk macosx` correctly resolves to Xcode's own MacOSX26.5.sdk, so
# we just ask CMake to resolve the "macosx" SDK name itself (same as
# `xcrun --sdk macosx --show-sdk-path`) instead of hardcoding a path.
# Only applied if the caller hasn't already set CMAKE_OSX_SYSROOT or
# SDKROOT explicitly.
set(CMAKE_OSX_ARCHITECTURES "arm64;x86_64" CACHE STRING "")
set(CMAKE_OSX_DEPLOYMENT_TARGET 14.2 CACHE STRING "")
if(NOT DEFINED CMAKE_OSX_SYSROOT AND NOT DEFINED ENV{SDKROOT})
    set(CMAKE_OSX_SYSROOT macosx CACHE STRING "")  # xcrun --sdk macosx -> selected Xcode's SDK
endif()
project(RoomMeshDriver CXX)
set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_OSX_DEPLOYMENT_TARGET 14.2)

enable_testing()
add_executable(ring_test tests/ring_test.cpp src/SharedRegion.cpp)
target_include_directories(ring_test PRIVATE src)
target_compile_options(ring_test PRIVATE -UNDEBUG)
add_test(NAME ring_test COMMAND ring_test)

set(DRIVER_FACTORY_UUID "9F3A7C2E-5B1D-4E8A-A6C4-2D7E91B0F5A3")
set(DRIVER_BUNDLE_ID "io.github.brohithkr.RoomMesh.Driver")
set(DRIVER_VERSION "1.0.0")

include(ExternalProject)
string(REPLACE ";" "|" ASPL_ARCHS "${CMAKE_OSX_ARCHITECTURES}")
ExternalProject_Add(libASPL_ext
    GIT_REPOSITORY https://github.com/gavv/libASPL.git
    GIT_TAG v3.1.2
    GIT_SHALLOW OFF   # libASPL's CMake calls `git describe --tags`
    LIST_SEPARATOR |
    # libASPL's own CMakeLists.txt uses TARGET-style add_custom_command()
    # calls with an empty BYPRODUCTS string for its (already-generated,
    # checked-in) .g.cpp files. With the Ninja generator this collapses to
    # a byproduct path of "." for several commands, and Ninja refuses to
    # generate ("build.ninja:...: . is defined as an output multiple
    # times"). Building libASPL itself with Unix Makefiles sidesteps that
    # generator bug; RoomMesh's own targets still build with Ninja.
    CMAKE_GENERATOR "Unix Makefiles"
    CMAKE_ARGS -DCMAKE_INSTALL_PREFIX=<INSTALL_DIR>
               -DCMAKE_BUILD_TYPE=Release
               -DCMAKE_OSX_ARCHITECTURES=${ASPL_ARCHS}
               -DCMAKE_OSX_DEPLOYMENT_TARGET=14.2
               -DCMAKE_OSX_SYSROOT=${CMAKE_OSX_SYSROOT}
               -DBUILD_TESTING=OFF
    BUILD_BYPRODUCTS <INSTALL_DIR>/lib/libASPL.a)
ExternalProject_Get_Property(libASPL_ext INSTALL_DIR)
set(ASPL_PREFIX ${INSTALL_DIR})
file(MAKE_DIRECTORY ${ASPL_PREFIX}/include)

configure_file(Info.plist.in ${CMAKE_BINARY_DIR}/Info.plist @ONLY)
add_library(RoomMesh MODULE src/Entry.cpp src/Handlers.cpp src/Devices.cpp src/SharedRegion.cpp)
add_dependencies(RoomMesh libASPL_ext)
target_include_directories(RoomMesh PRIVATE src)
target_include_directories(RoomMesh SYSTEM PRIVATE ${ASPL_PREFIX}/include)
target_link_libraries(RoomMesh PRIVATE ${ASPL_PREFIX}/lib/libASPL.a "-framework CoreAudio" "-framework CoreFoundation")
# The HAL loads the driver only through the CFPlugInFactories entry point
# (RoomMeshEntryPoint, exported explicitly with
# __attribute__((visibility("default"))) in Entry.cpp); hide everything
# else so the bundle doesn't leak libASPL/RoomMesh symbols. -fvisibility
# only affects RoomMesh's own object files, not the prebuilt libASPL.a
# (built by the ExternalProject above without that flag), so also tell the
# linker directly to export nothing but RoomMeshEntryPoint - that hides
# libASPL's symbols too.
target_compile_options(RoomMesh PRIVATE -fvisibility=hidden -fvisibility-inlines-hidden)
target_link_options(RoomMesh PRIVATE -Wl,-exported_symbol,_RoomMeshEntryPoint)
set_target_properties(RoomMesh PROPERTIES
    BUNDLE TRUE
    BUNDLE_EXTENSION driver
    OUTPUT_NAME RoomMesh
    MACOSX_BUNDLE_INFO_PLIST ${CMAKE_BINARY_DIR}/Info.plist)

# Verifies MakeDevice() actually produces Float32/48kHz streams (see the
# CRITICAL fix in Devices.cpp: AddStreamWithControlsAsync(Direction) alone
# would silently keep libASPL's 16-bit-int default format).
add_executable(device_format_test tests/device_format_test.cpp src/Devices.cpp)
add_dependencies(device_format_test libASPL_ext)
target_include_directories(device_format_test PRIVATE src)
target_include_directories(device_format_test SYSTEM PRIVATE ${ASPL_PREFIX}/include)
target_link_libraries(device_format_test PRIVATE ${ASPL_PREFIX}/lib/libASPL.a "-framework CoreAudio" "-framework CoreFoundation")
target_compile_options(device_format_test PRIVATE -UNDEBUG)
add_test(NAME device_format_test COMMAND device_format_test)
