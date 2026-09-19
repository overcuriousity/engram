/* Four functions over whisper.cpp, for the Rust side to call.
 *
 * whisper's own entry point takes a parameter struct of some forty fields by
 * value. Binding that means agreeing with its layout for ever; filling it in
 * here, in C++, means agreeing about four pointers and an int.
 */
#ifndef ENGRAM_WHISPER_H
#define ENGRAM_WHISPER_H

#ifdef __cplusplus
extern "C" {
#endif

struct engram_whisper;

/* NULL where the file is not a model this whisper reads. The CPU path: no GPU, no flash attention. */
struct engram_whisper * engram_whisper_open(const char * path);

/* The words in `n` samples of 16 kHz mono, as a malloc'd UTF-8 string; NULL on failure.
 * `lang` is an ISO-639-1 code, or NULL to let the model decide. Translation is never asked for. */
char * engram_whisper_run(struct engram_whisper * w, const float * samples, int n, const char * lang, int threads);

void engram_whisper_free_text(char * text);
void engram_whisper_close(struct engram_whisper * w);

#ifdef __cplusplus
}
#endif

#endif
