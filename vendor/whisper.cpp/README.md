# whisper.cpp, the part of it this crate compiles

Three source files from <https://github.com/ggml-org/whisper.cpp> at tag
`v1.9.2` (tag object `306c88f4d1286aec1bf96e544632897886af5501`), unmodified, under its MIT licence:
`src/whisper.cpp`, `src/whisper-arch.h`, `include/whisper.h`.

whisper.cpp is a model written over ggml. This crate does not build a ggml for
it. `build.rs` compiles these files against the ggml that `llama-cpp-sys-2`
has already built, and links that one: two ggmls in one library collide, and a
phone's library has room for one.

So the two are a matched pair and move together:

| | version | ggml |
|---|---|---|
| whisper.cpp, vendored here | v1.9.2 | written against 0.18.1 |
| `llama-cpp-sys-2`, pinned in `Cargo.toml` | =0.1.156 | carries 0.19.0 |

The vendored release is the newest one written against a ggml at or below
llama.cpp's; v1.9.3 wants 0.20. When the llama pin moves, look at
`ggml/CMakeLists.txt` in both and move this with it.

`engram_whisper.h` and `engram_whisper.cpp` are this repository's: four C
functions over whisper's API, so that nothing on the Rust side has to agree
with the layout of a parameter struct passed by value.
