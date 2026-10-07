cmake_minimum_required(VERSION 3.20)
project(pyrowave-sys LANGUAGES CXX C)

if(NOT DEFINED PYROWAVE_VENDOR_DIR)
    set(PYROWAVE_VENDOR_DIR ${CMAKE_CURRENT_SOURCE_DIR}/vendor/pyrowave)
endif()

# Mirror upstream pyrowave's standalone (top-level, no PYROWAVE_UTILS) Granite
# configuration at the vendored pin, but produce static archives only — the
# Rust crate links the C API statically.
set(GRANITE_RENDERER OFF CACHE BOOL "" FORCE)
set(GRANITE_VULKAN_SPIRV_CROSS OFF CACHE BOOL "" FORCE)
set(GRANITE_VULKAN_SHADER_MANAGER_RUNTIME_COMPILER OFF CACHE BOOL "" FORCE)
set(GRANITE_VULKAN_SYSTEM_HANDLES OFF CACHE BOOL "" FORCE)
set(GRANITE_SHADER_COMPILER_OPTIMIZE OFF CACHE BOOL "" FORCE)
set(GRANITE_POSITION_INDEPENDENT ON CACHE BOOL "" FORCE)
set(GRANITE_VULKAN_FOSSILIZE OFF CACHE BOOL "" FORCE)
set(GRANITE_SHIPPING ON CACHE BOOL "" FORCE)
set(GRANITE_PLATFORM "null" CACHE STRING "" FORCE)

add_subdirectory(${PYROWAVE_VENDOR_DIR}/Granite ${CMAKE_CURRENT_BINARY_DIR}/Granite EXCLUDE_FROM_ALL)
# Not top-level from here, so upstream's CMake only defines the static
# `pyrowave` codec library (shared lib / tests / install rules are skipped).
add_subdirectory(${PYROWAVE_VENDOR_DIR} ${CMAKE_CURRENT_BINARY_DIR}/pyrowave EXCLUDE_FROM_ALL)

# The C API as a static archive. Upstream only ships it as a SHARED target;
# compiling pyrowave_c.cpp (plus Granite's scaler, which the scaled encode
# entry point uses) against the same deps is equivalent minus the dllexport
# attributes (PYROWAVE_EXPORT_SYMBOLS stays undefined).
add_library(pyrowave-capi STATIC ${PYROWAVE_VENDOR_DIR}/pyrowave_c.cpp
            ${PYROWAVE_VENDOR_DIR}/Granite/video/scaler.cpp)
target_link_libraries(pyrowave-capi PRIVATE pyrowave granite-vulkan granite-math)
target_include_directories(pyrowave-capi PRIVATE
    ${PYROWAVE_VENDOR_DIR}/shaders ${PYROWAVE_VENDOR_DIR}/Granite/video)
