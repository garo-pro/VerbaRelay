cmake_minimum_required(VERSION 3.28)
project(verbarelay_engine LANGUAGES C CXX)

# Builds libvr_engine (a small C ABI) together with whisper.cpp and ggml as
# shared libraries. Every ggml compute backend is a separate module that is
# loaded at runtime, so one client build supports CPU, Vulkan and CUDA:
#
#   cmake -S engine -B build/engine -DCMAKE_BUILD_TYPE=Release \
#         -DVR_VULKAN=ON -DVR_CUDA=ON
#   cmake --build build/engine --config Release
#   cmake --install build/engine --prefix dist/engine
#
# All runtime files end up flat in the install prefix ("bin" on Windows, "lib"
# elsewhere), next to the client executable.

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

option(VR_VULKAN "Build the Vulkan backend module" OFF)
option(VR_CUDA "Build the CUDA backend module" OFF)
option(VR_CPU_ALL_VARIANTS "Build one CPU module per microarchitecture (picked at runtime)" ON)
set(VR_WHISPER_CPP_TAG "v1.9.5" CACHE STRING "whisper.cpp git tag to build")
set(VR_WHISPER_CPP_SOURCE_DIR "" CACHE PATH "Use a local whisper.cpp checkout instead of downloading one")

# Shared libraries with every backend as a runtime-loadable module.
set(BUILD_SHARED_LIBS ON CACHE BOOL "" FORCE)
set(GGML_BACKEND_DL ON CACHE BOOL "" FORCE)
set(GGML_NATIVE OFF CACHE BOOL "" FORCE)
set(GGML_CPU_ALL_VARIANTS ${VR_CPU_ALL_VARIANTS} CACHE BOOL "" FORCE)
set(GGML_VULKAN ${VR_VULKAN} CACHE BOOL "" FORCE)
set(GGML_CUDA ${VR_CUDA} CACHE BOOL "" FORCE)
set(WHISPER_BUILD_EXAMPLES OFF CACHE BOOL "" FORCE)
set(WHISPER_BUILD_TESTS OFF CACHE BOOL "" FORCE)
set(WHISPER_BUILD_SERVER OFF CACHE BOOL "" FORCE)
set(WHISPER_SDL2 OFF CACHE BOOL "" FORCE)
set(WHISPER_CURL OFF CACHE BOOL "" FORCE)

if (VR_CUDA AND NOT DEFINED CMAKE_CUDA_ARCHITECTURES)
    # Pascal (GTX 10xx) up to Blackwell (RTX 50xx), plus PTX for future GPUs.
    set(CMAKE_CUDA_ARCHITECTURES "61-virtual;75-real;86-real;89-real;120-real" CACHE STRING "" FORCE)
endif()

# Make every library find its siblings in its own directory on Linux.
set(CMAKE_BUILD_WITH_INSTALL_RPATH ON)
set(CMAKE_INSTALL_RPATH "$ORIGIN")

if (VR_WHISPER_CPP_SOURCE_DIR)
    add_subdirectory(${VR_WHISPER_CPP_SOURCE_DIR} whisper.cpp EXCLUDE_FROM_ALL)
else()
    include(FetchContent)
    FetchContent_Declare(
        whisper_cpp
        GIT_REPOSITORY https://github.com/ggml-org/whisper.cpp.git
        GIT_TAG        ${VR_WHISPER_CPP_TAG}
        GIT_SHALLOW    TRUE
        EXCLUDE_FROM_ALL
    )
    FetchContent_MakeAvailable(whisper_cpp)
endif()

add_library(vr_engine SHARED src/vr_engine.cpp)
target_include_directories(vr_engine PUBLIC include)
target_link_libraries(vr_engine PRIVATE whisper ggml ggml-base)
set_target_properties(vr_engine PROPERTIES CXX_VISIBILITY_PRESET hidden)

# Every backend module that was configured, so that `cmake --build` builds them
# even though whisper.cpp itself is EXCLUDE_FROM_ALL.
set(VR_BACKEND_TARGETS "")
foreach(candidate ggml-vulkan ggml-cuda)
    if (TARGET ${candidate})
        list(APPEND VR_BACKEND_TARGETS ${candidate})
    endif()
endforeach()
# CPU variants are named ggml-cpu-<arch>; with all variants off there is a single ggml-cpu.
include(cmake/collect_targets.cmake)
vr_collect_targets(VR_CPU_TARGETS "^ggml-cpu")
list(APPEND VR_BACKEND_TARGETS ${VR_CPU_TARGETS})
if (VR_BACKEND_TARGETS)
    add_dependencies(vr_engine ${VR_BACKEND_TARGETS})
endif()

if (WIN32)
    set(VR_RUNTIME_DEST bin)
else()
    set(VR_RUNTIME_DEST lib)
endif()

install(TARGETS vr_engine whisper ggml ggml-base ${VR_BACKEND_TARGETS}
    RUNTIME DESTINATION ${VR_RUNTIME_DEST}
    LIBRARY DESTINATION ${VR_RUNTIME_DEST}
    ARCHIVE EXCLUDE_FROM_ALL
    PUBLIC_HEADER EXCLUDE_FROM_ALL)
