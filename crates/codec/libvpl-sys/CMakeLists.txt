# Thin driver over the vendored Intel VPL tree (vendor/libvpl): build the
# dispatcher as a static archive, nothing else. Replaces upstream's top-level
# CMakeLists.txt (which also wires examples/env/install machinery).
cmake_minimum_required(VERSION 3.13.0)
if(MSVC)
  cmake_policy(SET CMP0091 NEW)
endif()

file(STRINGS "vendor/libvpl/version.txt" version_txt)
project(vpl VERSION ${version_txt})

set(BUILD_SHARED_LIBS OFF)
set(BUILD_TESTS OFF)
set(INSTALL_DEV OFF)
set(INSTALL_LIB OFF)
set(INSTALL_EXAMPLES OFF)
# Defines ONEVPL_EXPERIMENTAL inside the dispatcher — must match the bindgen
# flag in build.rs (the D3D11 import API lives behind it).
set(BUILD_EXPERIMENTAL ON)

set(CMAKE_POSITION_INDEPENDENT_CODE true)
# One predictable place for the archive; MSVC multi-config generators still
# append a per-config subdir (Release/) — build.rs searches both.
set(CMAKE_ARCHIVE_OUTPUT_DIRECTORY ${PROJECT_BINARY_DIR})

# api/ first: it exports API_VERSION_MAJOR/MINOR + the vpl-api interface
# target into this scope, which libvpl/ then consumes (same ordering contract
# as upstream's top-level).
add_subdirectory(vendor/libvpl/api)
add_subdirectory(vendor/libvpl/libvpl)
