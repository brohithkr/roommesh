cmake_minimum_required(VERSION 3.20)
# NOTE (toolchain adaptation): on this machine, CommandLineTools' default
# "MacOSX.sdk" symlink points at MacOSX27.0.sdk, whose .tbd stub libraries
# advertise an architecture triple ("arm64e.x1-macos") that this
# CommandLineTools' own linker/tapi (ld-1267 / tapi-2100.0.2.6) cannot
# parse ("tapi error: malformed file ... unknown architecture"), so even a
# trivial `int main(){}` fails to link against the default SDK - for any
# arch, not just x86_64. The CommandLineTools bundle also ships an older,
# working MacOSX26.5.sdk. We pin CMAKE_OSX_SYSROOT to it so the universal
# arm64+x86_64 build (as specified by the plan) links correctly.
set(CMAKE_OSX_ARCHITECTURES "arm64;x86_64" CACHE STRING "")
set(CMAKE_OSX_DEPLOYMENT_TARGET 14.2 CACHE STRING "")
if(NOT DEFINED CMAKE_OSX_SYSROOT OR CMAKE_OSX_SYSROOT STREQUAL "")
    if(EXISTS "/Library/Developer/CommandLineTools/SDKs/MacOSX26.5.sdk")
        set(CMAKE_OSX_SYSROOT "/Library/Developer/CommandLineTools/SDKs/MacOSX26.5.sdk" CACHE STRING "")
    endif()
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
add_library(RoomMesh MODULE src/Entry.cpp src/Handlers.cpp src/SharedRegion.cpp)
add_dependencies(RoomMesh libASPL_ext)
target_include_directories(RoomMesh PRIVATE src ${ASPL_PREFIX}/include)
target_link_libraries(RoomMesh PRIVATE ${ASPL_PREFIX}/lib/libASPL.a "-framework CoreAudio" "-framework CoreFoundation")
set_target_properties(RoomMesh PROPERTIES
    BUNDLE TRUE
    BUNDLE_EXTENSION driver
    OUTPUT_NAME RoomMesh
    MACOSX_BUNDLE_INFO_PLIST ${CMAKE_BINARY_DIR}/Info.plist)
