/*
 * VerbaRelay engine: a small, stable C ABI on top of whisper.cpp.
 *
 * The Rust client never links against whisper.cpp directly. It loads this
 * library at runtime (dlopen / LoadLibrary), so the client executable starts
 * even when the native libraries are missing or broken, and so the struct
 * layouts of whisper.cpp never leak into Rust code.
 *
 * All strings are UTF-8. All functions are safe to call from one thread at a
 * time per context.
 */
#ifndef VR_ENGINE_H
#define VR_ENGINE_H

#include <stdint.h>

#ifdef _WIN32
#    define VR_API __declspec(dllexport)
#else
#    define VR_API __attribute__((visibility("default")))
#endif

#ifdef __cplusplus
extern "C" {
#endif

/* Bumped whenever a signature in this header changes. */
#define VR_ENGINE_ABI_VERSION 1

typedef struct vr_context vr_context;

/* Called for every finished segment. Times are in milliseconds. */
typedef void (*vr_segment_callback)(void * user_data, int64_t t0_ms, int64_t t1_ms, const char * text);

VR_API int vr_engine_abi_version(void);

/* Version string of the bundled whisper.cpp. */
VR_API const char * vr_engine_whisper_version(void);

/*
 * Loads every ggml backend module found in `dir` (CPU variants, Vulkan, CUDA, ...).
 * Missing or unloadable modules are skipped. Returns the number of registered
 * devices afterwards. Safe to call more than once.
 */
VR_API int vr_engine_load_backends(const char * dir);

/*
 * Writes a JSON array describing the registered devices into `buf`:
 * [{"index":0,"name":"...","description":"...","backend":"Vulkan","type":"gpu","memory_total":123,"memory_free":45}]
 * Returns the number of bytes needed (excluding the terminator). If the result is
 * >= buf_len the output was truncated.
 */
VR_API int vr_engine_list_devices(char * buf, int buf_len);

/*
 * Loads a model. `backend` is one of "cpu", "vulkan", "cuda".
 * Returns NULL on failure and writes a message into `err`.
 */
VR_API vr_context * vr_engine_init(const char * model_path, const char * backend, int flash_attn, char * err, int err_len);

/*
 * Transcribes 16 kHz mono float samples.
 * `language` may be NULL or "auto" for detection.
 * `abort_flag` may be NULL; when it points to a non-zero value the run is aborted.
 * Returns 0 on success.
 */
VR_API int vr_engine_transcribe(
    vr_context *        ctx,
    const float *       samples,
    int                 n_samples,
    const char *        language,
    int                 translate,
    int                 n_threads,
    vr_segment_callback callback,
    void *              user_data,
    volatile int32_t *  abort_flag,
    char *              err,
    int                 err_len);

/* Language detected (or forced) during the last successful run, e.g. "de". */
VR_API const char * vr_engine_last_language(vr_context * ctx);

VR_API void vr_engine_free(vr_context * ctx);

#ifdef __cplusplus
}
#endif

#endif /* VR_ENGINE_H */
