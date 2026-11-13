package io.github.overcuriousity.engram.ui

import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.wrapContentHeight
import androidx.compose.ui.unit.dp

/** A download's size as a person reads one: decimal, whole megabytes, one decimal from a gigabyte. */
fun sizeWords(bytes: Long): String =
    if (bytes >= 1_000_000_000) "%.1f GB".format(java.util.Locale.ROOT, bytes / 1e9)
    else "${Math.round(bytes / 1e6)} MB"

/** The manifest's 48 dp touch target, content centred in it. */
fun androidx.compose.ui.Modifier.touchTarget() =
    heightIn(min = 48.dp).wrapContentHeight(androidx.compose.ui.Alignment.CenterVertically)
