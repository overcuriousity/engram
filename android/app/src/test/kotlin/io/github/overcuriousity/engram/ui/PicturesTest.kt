package io.github.overcuriousity.engram.ui

import android.graphics.Bitmap
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.material3.MaterialTheme
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.asAndroidBitmap
import androidx.compose.ui.test.captureToImage
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onRoot
import io.github.overcuriousity.engram.core.read.Hit
import io.github.overcuriousity.engram.core.read.Reach
import io.github.overcuriousity.engram.core.read.Read
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode
import java.io.File

/**
 * Pictures of the screens' parts, written to `build/pictures/`. Not
 * assertions: there is no device in the loop while this is being built, and a
 * layout has to be looked at by somebody. Skipped unless asked for, because a
 * test that only writes files has no business slowing the suite.
 */
@RunWith(RobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
@Config(sdk = [35], qualifiers = "w411dp-h891dp-xxhdpi")
class PicturesTest {
    @get:Rule val compose = createComposeRule()

    private fun save(name: String) {
        if (System.getProperty("engram.pictures") == null) return
        val dir = File("build/pictures").apply { mkdirs() }
        val bmp = compose.onRoot().captureToImage().asAndroidBitmap()
        File(dir, "$name.png").outputStream().use { bmp.compress(Bitmap.CompressFormat.PNG, 100, it) }
    }

    @Test fun aResultList() {
        val hits = listOf(
            Hit("a", title = "Qdrant payload filters", text = "Filters narrow a search before the vectors are compared, and a payload index is what makes that fast.", primed = true),
            Hit("b", title = "Kapitel 3", borrowedName = true, text = "Der Vorgang setzt voraus, dass das Journal noch steht und nicht rotiert wurde."),
            Hit("c", title = "Reindexing after a model change", text = "Every artifact is embedded again; the old vectors are kept until the new ones are complete.", dueIn = "in 2 h"),
            Hit("d", title = "Backup rotation", text = "Seven dailies, four weeklies.", pastCliff = true, modelWritten = true, originCount = 3),
            Hit("e", title = "Pay rent", text = "Pay rent by the third.", pastCliff = true, weak = true, retired = true),
        )
        compose.setContent {
            EngramTheme {
                Column(Modifier.fillMaxWidth().background(MaterialTheme.colorScheme.background)) {
                    ReadFrame(ReadState(Read(hits, 1_000L, Reach.Unreachable, loading = false)) {}) { Rail(railOf(it)) {} }
                }
            }
        }
        save("results-unreachable")
    }

    @Test fun nothingClose() {
        val hits = listOf(Hit("a", title = "One", text = "x y z", weak = true), Hit("b", text = "an untitled passage that opens like this", weak = true))
        compose.setContent {
            EngramTheme { Column(Modifier.fillMaxWidth().background(MaterialTheme.colorScheme.background)) { Rail(railOf(hits)) {} } }
        }
        save("results-all-loose")
    }
}
