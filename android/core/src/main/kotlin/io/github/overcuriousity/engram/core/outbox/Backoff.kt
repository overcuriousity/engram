package io.github.overcuriousity.engram.core.outbox

/** After the n-th failed attempt, wait this long. Ends flat at two hours: a row is never given up on. */
object Backoff {
    private val LADDER = longArrayOf(30_000, 120_000, 600_000, 1_800_000, 3_600_000, 7_200_000)
    fun delayMs(attempts: Int): Long = LADDER[(attempts - 1).coerceIn(0, LADDER.size - 1)]
}
