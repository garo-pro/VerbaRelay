// Implementation of the VerbaRelay engine ABI. See include/vr_engine.h.

#include "vr_engine.h"

#include "ggml-backend.h"
#include "whisper.h"

#include <algorithm>
#include <atomic>
#include <cstdio>
#include <cstring>
#include <mutex>
#include <string>

struct vr_context {
    whisper_context * ctx = nullptr;
    std::string       last_language;
};

namespace {

std::mutex g_load_mutex;

void write_error(char * err, int err_len, const std::string & message) {
    if (err == nullptr || err_len <= 0) {
        return;
    }
    const size_t n = std::min(message.size(), static_cast<size_t>(err_len - 1));
    std::memcpy(err, message.data(), n);
    err[n] = '\0';
}

std::string json_escape(const char * s) {
    std::string out;
    if (s == nullptr) {
        return out;
    }
    for (const char * p = s; *p; ++p) {
        const unsigned char c = static_cast<unsigned char>(*p);
        switch (c) {
            case '"':  out += "\\\""; break;
            case '\\': out += "\\\\"; break;
            case '\n': out += "\\n"; break;
            case '\r': out += "\\r"; break;
            case '\t': out += "\\t"; break;
            default:
                if (c < 0x20) {
                    char tmp[8];
                    std::snprintf(tmp, sizeof(tmp), "\\u%04x", c);
                    out += tmp;
                } else {
                    out += static_cast<char>(c);
                }
        }
    }
    return out;
}

const char * device_type_name(enum ggml_backend_dev_type type) {
    switch (type) {
        case GGML_BACKEND_DEVICE_TYPE_CPU:   return "cpu";
        case GGML_BACKEND_DEVICE_TYPE_GPU:   return "gpu";
        case GGML_BACKEND_DEVICE_TYPE_IGPU:  return "igpu";
        case GGML_BACKEND_DEVICE_TYPE_ACCEL: return "accel";
        default:                             return "other";
    }
}

bool is_gpu_type(enum ggml_backend_dev_type type) {
    return type == GGML_BACKEND_DEVICE_TYPE_GPU || type == GGML_BACKEND_DEVICE_TYPE_IGPU;
}

bool equals_ignore_case(const char * a, const char * b) {
    if (a == nullptr || b == nullptr) {
        return false;
    }
    while (*a && *b) {
        const char ca = (*a >= 'A' && *a <= 'Z') ? static_cast<char>(*a + 32) : *a;
        const char cb = (*b >= 'A' && *b <= 'Z') ? static_cast<char>(*b + 32) : *b;
        if (ca != cb) {
            return false;
        }
        ++a;
        ++b;
    }
    return *a == *b;
}

// whisper.cpp selects the GPU by counting GPU and IGPU devices in registration
// order (see whisper_backend_init_gpu). We mirror that logic to find the ordinal
// of the first GPU device that belongs to the requested backend, preferring a
// dedicated GPU over an integrated one.
int find_gpu_ordinal(const char * backend_name) {
    int ordinal         = 0;
    int igpu_candidate  = -1;
    for (size_t i = 0; i < ggml_backend_dev_count(); ++i) {
        ggml_backend_dev_t dev  = ggml_backend_dev_get(i);
        const auto         type = ggml_backend_dev_type(dev);
        if (!is_gpu_type(type)) {
            continue;
        }
        ggml_backend_reg_t reg = ggml_backend_dev_backend_reg(dev);
        if (equals_ignore_case(ggml_backend_reg_name(reg), backend_name)) {
            if (type == GGML_BACKEND_DEVICE_TYPE_GPU) {
                return ordinal;
            }
            if (igpu_candidate < 0) {
                igpu_candidate = ordinal;
            }
        }
        ++ordinal;
    }
    return igpu_candidate;
}

struct callback_state {
    vr_segment_callback callback;
    void *              user_data;
};

void on_new_segment(whisper_context * ctx, whisper_state * /*state*/, int n_new, void * user_data) {
    auto *    cb    = static_cast<callback_state *>(user_data);
    const int total = whisper_full_n_segments(ctx);
    for (int i = total - n_new; i < total; ++i) {
        // whisper.cpp timestamps are in units of 10 ms.
        const int64_t t0 = whisper_full_get_segment_t0(ctx, i) * 10;
        const int64_t t1 = whisper_full_get_segment_t1(ctx, i) * 10;
        cb->callback(cb->user_data, t0, t1, whisper_full_get_segment_text(ctx, i));
    }
}

bool on_abort(void * user_data) {
    auto * flag = static_cast<volatile int32_t *>(user_data);
    return flag != nullptr && *flag != 0;
}

void quiet_log(enum ggml_log_level level, const char * text, void * /*user_data*/) {
    if (level >= GGML_LOG_LEVEL_WARN) {
        std::fputs(text, stderr);
    }
}

}  // namespace

extern "C" {

int vr_engine_abi_version(void) {
    return VR_ENGINE_ABI_VERSION;
}

const char * vr_engine_whisper_version(void) {
    return whisper_version();
}

int vr_engine_load_backends(const char * dir) {
    std::lock_guard<std::mutex> lock(g_load_mutex);
    whisper_log_set(quiet_log, nullptr);
    if (ggml_backend_dev_count() == 0) {
        if (dir != nullptr && dir[0] != '\0') {
            ggml_backend_load_all_from_path(dir);
        } else {
            ggml_backend_load_all();
        }
    }
    return static_cast<int>(ggml_backend_dev_count());
}

int vr_engine_list_devices(char * buf, int buf_len) {
    std::string json = "[";
    for (size_t i = 0; i < ggml_backend_dev_count(); ++i) {
        ggml_backend_dev_t dev = ggml_backend_dev_get(i);
        size_t             free_mem  = 0;
        size_t             total_mem = 0;
        ggml_backend_dev_memory(dev, &free_mem, &total_mem);
        ggml_backend_reg_t reg = ggml_backend_dev_backend_reg(dev);

        if (i > 0) {
            json += ",";
        }
        json += "{\"index\":" + std::to_string(i);
        json += ",\"name\":\"" + json_escape(ggml_backend_dev_name(dev)) + "\"";
        json += ",\"description\":\"" + json_escape(ggml_backend_dev_description(dev)) + "\"";
        json += ",\"backend\":\"" + json_escape(ggml_backend_reg_name(reg)) + "\"";
        json += ",\"type\":\"" + std::string(device_type_name(ggml_backend_dev_type(dev))) + "\"";
        json += ",\"memory_total\":" + std::to_string(total_mem);
        json += ",\"memory_free\":" + std::to_string(free_mem) + "}";
    }
    json += "]";

    if (buf != nullptr && buf_len > 0) {
        const size_t n = std::min(json.size(), static_cast<size_t>(buf_len - 1));
        std::memcpy(buf, json.data(), n);
        buf[n] = '\0';
    }
    return static_cast<int>(json.size());
}

vr_context * vr_engine_init(const char * model_path, const char * backend, int flash_attn, char * err, int err_len) {
    if (model_path == nullptr || backend == nullptr) {
        write_error(err, err_len, "model path and backend are required");
        return nullptr;
    }
    if (ggml_backend_dev_count() == 0) {
        write_error(err, err_len, "no ggml backends are loaded; call vr_engine_load_backends first");
        return nullptr;
    }

    whisper_context_params params = whisper_context_default_params();
    params.flash_attn             = flash_attn != 0;

    if (equals_ignore_case(backend, "cpu")) {
        params.use_gpu = false;
    } else {
        const char * reg_name = equals_ignore_case(backend, "cuda")   ? "CUDA"
                                : equals_ignore_case(backend, "vulkan") ? "Vulkan"
                                                                        : nullptr;
        if (reg_name == nullptr) {
            write_error(err, err_len, std::string("unknown backend: ") + backend);
            return nullptr;
        }
        const int ordinal = find_gpu_ordinal(reg_name);
        if (ordinal < 0) {
            write_error(err, err_len, std::string("no GPU device available for backend ") + reg_name);
            return nullptr;
        }
        params.use_gpu    = true;
        params.gpu_device = ordinal;
    }

    whisper_context * ctx = whisper_init_from_file_with_params(model_path, params);
    if (ctx == nullptr) {
        write_error(err, err_len, std::string("failed to load model: ") + model_path);
        return nullptr;
    }

    auto * result = new vr_context();
    result->ctx   = ctx;
    return result;
}

int vr_engine_transcribe(
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
    int                 err_len) {
    if (ctx == nullptr || ctx->ctx == nullptr || samples == nullptr || n_samples <= 0) {
        write_error(err, err_len, "invalid arguments");
        return -1;
    }

    whisper_full_params params = whisper_full_default_params(WHISPER_SAMPLING_BEAM_SEARCH);
    params.print_progress      = false;
    params.print_realtime      = false;
    params.print_special       = false;
    params.print_timestamps    = false;
    params.translate           = translate != 0;
    params.n_threads           = n_threads > 0 ? n_threads : params.n_threads;
    params.language            = (language == nullptr || language[0] == '\0') ? "auto" : language;
    params.detect_language     = false;

    callback_state state{ callback, user_data };
    if (callback != nullptr) {
        params.new_segment_callback           = on_new_segment;
        params.new_segment_callback_user_data = &state;
    }
    if (abort_flag != nullptr) {
        params.abort_callback           = on_abort;
        params.abort_callback_user_data = const_cast<int32_t *>(abort_flag);
    }

    const int rc = whisper_full(ctx->ctx, params, samples, n_samples);
    if (rc != 0) {
        if (abort_flag != nullptr && *abort_flag != 0) {
            write_error(err, err_len, "aborted");
        } else {
            write_error(err, err_len, "whisper_full failed with code " + std::to_string(rc));
        }
        return rc;
    }

    const int lang_id  = whisper_full_lang_id(ctx->ctx);
    const char * lang  = lang_id >= 0 ? whisper_lang_str(lang_id) : nullptr;
    ctx->last_language = lang != nullptr ? lang : "";
    return 0;
}

const char * vr_engine_last_language(vr_context * ctx) {
    return ctx != nullptr ? ctx->last_language.c_str() : "";
}

void vr_engine_free(vr_context * ctx) {
    if (ctx == nullptr) {
        return;
    }
    if (ctx->ctx != nullptr) {
        whisper_free(ctx->ctx);
    }
    delete ctx;
}

}  // extern "C"
