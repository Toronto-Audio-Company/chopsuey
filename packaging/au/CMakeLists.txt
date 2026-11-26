# Wraps the CLAP build (target/bundled/chopsuey.clap) as an Audio Unit v2
# instrument with free-audio/clap-wrapper. The .clap is copied into the
# component's Contents/PlugIns, where the wrapper loads it from, so the
# .component is self-contained.
#
# Don't run this directly: packaging/au/build.sh passes the variables below.
#   CHOPSUEY_CLAP            path to the built chopsuey.clap
#   CHOPSUEY_VERSION         version (from Cargo.toml)
#   CMAKE_OSX_ARCHITECTURES  match the .clap ("arm64" or "arm64;x86_64")
cmake_minimum_required(VERSION 3.21)

set(CMAKE_OSX_DEPLOYMENT_TARGET "11.0" CACHE STRING "Minimum macOS version")
project(chopsuey_au VERSION ${CHOPSUEY_VERSION} LANGUAGES C CXX OBJC OBJCXX)
set(CMAKE_CXX_STANDARD 17)

if (NOT EXISTS "${CHOPSUEY_CLAP}/Contents/Info.plist")
  message(FATAL_ERROR "CHOPSUEY_CLAP must point at a built chopsuey.clap bundle")
endif()

include(FetchContent)
# Pinned to v0.18.0. It downloads the CLAP SDK and Apple's AudioUnitSDK
# itself (into the build directory).
set(CLAP_WRAPPER_DOWNLOAD_DEPENDENCIES ON CACHE BOOL "" FORCE)
FetchContent_Declare(clap-wrapper
  GIT_REPOSITORY https://github.com/free-audio/clap-wrapper.git
  GIT_TAG c3a2531a5e66572b4246fc64271e2103d0de37bb) # v0.18.0
FetchContent_MakeAvailable(clap-wrapper)

add_library(chopsuey_auv2 MODULE)
target_add_auv2_wrapper(
  TARGET chopsuey_auv2
  OUTPUT_NAME "chopsuey"
  BUNDLE_IDENTIFIER "com.torontoaudiocompany.chopsuey" # build.sh sets ...chopsuey.component
  BUNDLE_VERSION "${CHOPSUEY_VERSION}"
  MANUFACTURER_NAME "Toronto Audio Company"
  MANUFACTURER_CODE "TAud"
  SUBTYPE_CODE "Chsy"
  INSTRUMENT_TYPE "aumu"
  MACOSX_EMBEDDED_CLAP_LOCATION "${CHOPSUEY_CLAP}"
)
