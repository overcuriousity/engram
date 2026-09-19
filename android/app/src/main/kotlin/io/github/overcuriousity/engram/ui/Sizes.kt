package io.github.overcuriousity.engram.ui

/** A download's size as a person reads one: decimal, whole megabytes, one decimal from a gigabyte. */
fun sizeWords(bytes: Long): String =
    if (bytes >= 1_000_000_000) "%.1f GB".format(java.util.Locale.ROOT, bytes / 1e9)
    else "${Math.round(bytes / 1e6)} MB"
