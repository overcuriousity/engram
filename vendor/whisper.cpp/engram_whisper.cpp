#include "engram_whisper.h"
#include "whisper.h"

#include <cstdlib>
#include <cstring>
#include <string>

struct engram_whisper {
    whisper_context * ctx;
};

// whisper.cpp and ggml report every tensor they load to stderr. None of that
// is this program's to show.
static void quiet(enum ggml_log_level, const char *, void *) {}

struct engram_whisper * engram_whisper_open(const char * path) {
    whisper_log_set(quiet, nullptr);
    whisper_context_params cp = whisper_context_default_params();
    cp.use_gpu    = false;
    cp.flash_attn = false;
    whisper_context * ctx = whisper_init_from_file_with_params(path, cp);
    if (ctx == nullptr) {
        return nullptr;
    }
    return new engram_whisper{ctx};
}

char * engram_whisper_run(struct engram_whisper * w, const float * samples, int n, const char * lang, int threads) {
    if (w == nullptr || w->ctx == nullptr) {
        return nullptr;
    }
    whisper_full_params p = whisper_full_default_params(WHISPER_SAMPLING_GREEDY);
    p.n_threads        = threads > 0 ? threads : 1;
    p.print_progress   = false;
    p.print_realtime   = false;
    p.print_special    = false;
    p.print_timestamps = false;
    p.no_timestamps    = true;
    p.translate        = false;
    p.detect_language  = false;
    // "auto" is whisper's word for deciding from the audio.
    p.language         = lang != nullptr ? lang : "auto";

    if (whisper_full(w->ctx, p, samples, n) != 0) {
        return nullptr;
    }
    std::string out;
    const int segments = whisper_full_n_segments(w->ctx);
    for (int i = 0; i < segments; ++i) {
        const char * text = whisper_full_get_segment_text(w->ctx, i);
        if (text != nullptr) {
            out += text;
        }
    }
    char * copy = static_cast<char *>(std::malloc(out.size() + 1));
    if (copy == nullptr) {
        return nullptr;
    }
    std::memcpy(copy, out.c_str(), out.size() + 1);
    return copy;
}

void engram_whisper_free_text(char * text) {
    std::free(text);
}

void engram_whisper_close(struct engram_whisper * w) {
    if (w == nullptr) {
        return;
    }
    if (w->ctx != nullptr) {
        whisper_free(w->ctx);
    }
    delete w;
}
