package io.github.overcuriousity.engram.core.contained

enum class Role { embed, rerank, ask, speech }

/** One model as the app knows it: enough to fetch it, check it, and say what it is and whose. */
data class Model(
    val role: Role,
    val name: String,
    val file: String,
    val url: String,
    val sha256: String,
    val bytes: Long,
    val licence: String,
    /** Where the licence asks for a notice to be shown, the address of its terms. */
    val terms: String? = null,
    val default: Boolean = true,
)

/**
 * What can be downloaded. Changing a default is a change to this file.
 *
 * Every value was read from the publisher's API on 2026-09-19. Each URL names
 * a revision, so a later upload cannot turn a good hash into a failed download.
 * No reranker: the device pass chooses one, or chooses none.
 */
object ModelManifest {
    private fun hf(repo: String, rev: String, file: String) = "https://huggingface.co/$repo/resolve/$rev/$file"

    val all = listOf(
        Model(
            Role.embed, "EmbeddingGemma 300M", "embeddinggemma-300m-q8_0.gguf",
            hf("ggml-org/embeddinggemma-300M-GGUF", "0f741b5a6585bd53aeb15cd1372c56f2a0f65e12", "embeddinggemma-300M-Q8_0.gguf"),
            "b5ce9d77a3fc4b3b39ccb5643c36777911cc4eb46a66962eadfa3f5f60490d63", 333_590_944,
            "Gemma Terms of Use", terms = "https://ai.google.dev/gemma/terms",
        ),
        Model(
            Role.ask, "Qwen3.5-2B", "qwen3.5-2b-q4_k_m.gguf",
            hf("unsloth/Qwen3.5-2B-GGUF", "f6d5376be1edb4d416d56da11e5397a961aca8ae", "Qwen3.5-2B-Q4_K_M.gguf"),
            "aaf42c8b7c3cab2bf3d69c355048d4a0ee9973d48f16c731c0520ee914699223", 1_280_835_840, "Apache 2.0",
        ),
        Model(
            Role.ask, "Qwen3.5-4B", "qwen3.5-4b-q4_k_m.gguf",
            hf("unsloth/Qwen3.5-4B-GGUF", "e87f176479d0855a907a41277aca2f8ee7a09523", "Qwen3.5-4B-Q4_K_M.gguf"),
            "00fe7986ff5f6b463e62455821146049db6f9313603938a70800d1fb69ef11a4", 2_740_937_888, "Apache 2.0",
            default = false,
        ),
        Model(
            Role.speech, "Whisper small", "whisper-small-q5_1.bin",
            hf("ggerganov/whisper.cpp", "5359861c739e955e79d9a303bcbc70fb988958b1", "ggml-small-q5_1.bin"),
            "ae85e4a935d7a567bd102fe55afc16bb595bdb618e11b2fc7591bc08120411bb", 190_085_487, "MIT",
        ),
    )

    /** What contained mode cannot start without. */
    val required: List<Model> get() = all.filter { it.role == Role.embed }
    fun defaultFor(role: Role): Model? = all.firstOrNull { it.role == role && it.default }
}
