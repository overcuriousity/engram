package io.github.overcuriousity.engram.ui

import android.graphics.Bitmap
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.asAndroidBitmap
import androidx.compose.ui.test.captureToImage
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onRoot
import androidx.compose.ui.unit.dp
import io.github.overcuriousity.engram.core.read.Beside
import io.github.overcuriousity.engram.core.read.Hit
import io.github.overcuriousity.engram.core.read.Pair
import io.github.overcuriousity.engram.core.read.PairSide
import io.github.overcuriousity.engram.core.read.SetAsideRow
import io.github.overcuriousity.engram.core.read.Reach
import io.github.overcuriousity.engram.core.read.Read
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode
import java.io.File

private typealias St = io.github.overcuriousity.engram.core.contained.Progress.State

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

    /** Home's one box, with the verbs under it. */
    @Test fun theBox() {
        compose.setContent {
            EngramTheme {
                Column(Modifier.fillMaxWidth().background(MaterialTheme.colorScheme.background)) {
                    HomeBox(text = "", onText = {})
                }
            }
        }
        save("home-box")
    }

    /** An artifact's text, as text: the note from the screenshot that used to show its asterisks. */
    @Test fun anArtifactRendered() {
        compose.setContent {
            EngramTheme {
                Column(Modifier.fillMaxWidth().background(MaterialTheme.colorScheme.background)) {
                    Decisions(
                        chunk = io.github.overcuriousity.engram.core.read.Chunk(id = "a", text = "", status = "active"),
                        decided = null, onVerify = {}, onHide = {}, onReactivate = {}, onDelete = {}, onUndo = {}, onWinner = {},
                    )
                    Markdown(
                        "## Öffnungszeiten Wertstoffhof Bad Aibling\n\n**Adresse:** Thürhamer Straße 21a, 83043 Bad Aibling\n\n" +
                            "**Öffnungszeiten:**\n- Montag: geschlossen\n- Dienstag: 08:00–12:30 Uhr und 14:00–18:00 Uhr\n- Samstag: 08:00–13:00 Uhr\n\n" +
                            "**Quelle:** https://www.rathaus-bad-aibling.de/adresse/Wertstoffhof-address509",
                        Modifier.padding(16.dp, 8.dp),
                    )
                }
            }
        }
        save("artifact")
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

    @Test fun aPairCard() {
        val cards = listOf(
            PairCard(
                Pair(
                    id = 1,
                    percent = 91,
                    a = PairSide("art-a", "Qdrant payload filters", named = true, excerpt = "Filters narrow a search before the vectors are compared, and a payload index is what makes that fast."),
                    b = PairSide("art-b", "the same thing, said in a note I made later", named = false, excerpt = "payload filtering happens first; without an index on the field it is a full scan."),
                    finding = "both say filtering happens before the comparison; the second adds what it costs without an index",
                    mergeable = true,
                    keeps = "art-a",
                ),
                siblings = 3,
            ),
        )
        compose.setContent {
            EngramTheme {
                Column(Modifier.fillMaxWidth().background(MaterialTheme.colorScheme.background)) {
                    PairReview(cards, onAnswer = { _, _ -> "row" }, onUndo = { true }, onArtifact = {}, modifier = Modifier)
                }
            }
        }
        save("pair-card")
    }

    @Test fun anUnjudgedPairCard() {
        val cards = listOf(
            PairCard(
                Pair(
                    id = 2,
                    percent = 88,
                    a = PairSide("art-a", "Timeout 0", named = true, excerpt = "the timeout is 30 seconds"),
                    b = PairSide("art-b", "Timeout 1", named = true, excerpt = "the timeout is 90 seconds"),
                    unjudged = true,
                    mergeable = false,
                ),
                siblings = 1,
            ),
        )
        compose.setContent {
            EngramTheme {
                Column(Modifier.fillMaxWidth().background(MaterialTheme.colorScheme.background)) {
                    PairReview(cards, onAnswer = { _, _ -> "row" }, onUndo = { true }, onArtifact = {}, modifier = Modifier)
                }
            }
        }
        save("pair-card-unjudged")
    }

    @Test fun theJournal() {
        val rows = listOf(
            SetAsideRow(
                kind = "merged", subjectId = "m1", artifactId = "m1",
                label = "Payload filtering, in one place", named = true,
                subtitle = "17 Sep 19:01",
                why = "written from 2 artifacts, which are still stored — undoing brings them back and retires this",
                beside = listOf(Beside("s1", "c1", "Payload filters 0", true), Beside("s2", "c1", "Payload filters 1", true)),
                caveat = "a source has since been deleted",
            ),
            SetAsideRow(
                kind = "parked", subjectId = "c9",
                label = "meeting-notes.pdf", named = true, subtitle = "184 kB",
                why = "96% the same as the capture beside it, so nothing has been spent on reading it yet",
                beside = listOf(Beside("", "c8", "meeting notes (2).pdf", true)),
            ),
        )
        compose.setContent {
            EngramTheme {
                Column(Modifier.fillMaxWidth().background(MaterialTheme.colorScheme.background)) {
                    Journal(rows, capped = false, onAction = { _, _ -> "row" }, onUndo = { true }, onOpen = {}, modifier = Modifier)
                }
            }
        }
        save("journal")
    }

    /** A new install's first screen. */
    @Test fun theChooser() {
        compose.setContent {
            EngramTheme {
                Column(Modifier.fillMaxWidth().background(MaterialTheme.colorScheme.background)) { ModeChooser(true, "334 MB", {}, {}) }
            }
        }
        save("mode-chooser")
    }

    /** A model's row in each thing it can be, then the offer on Ask. */
    @Test fun modelsAndTheAskOffer() {
        val all = io.github.overcuriousity.engram.core.contained.ModelManifest.all
        val embed = all[0]; val ask = all[1]; val large = all[2]
        fun p(bytes: Long, of: Long, st: io.github.overcuriousity.engram.core.contained.Progress.State, e: String? = null) =
            io.github.overcuriousity.engram.core.contained.Progress(bytes, of, st, e)
        compose.setContent {
            EngramTheme {
                Column(Modifier.fillMaxWidth().background(MaterialTheme.colorScheme.background).padding(16.dp)) {
                    ModelRow(embed, p(embed.bytes, embed.bytes, St.Done), true, {}, {}, {})
                    ModelRow(ask, p(512_000_000, ask.bytes, St.Running), false, {}, {}, {})
                    ModelRow(large, p(0, large.bytes, St.Waiting), false, {}, {}, {})
                    ModelRow(ask, p(0, ask.bytes, St.Failed, "Qwen3.5-2B did not verify"), false, {}, {}, {})
                    AskOfferPane({ ModelRow(ask, p(0, ask.bytes, St.Idle), false, {}, {}, {}); ModelRow(large, p(0, large.bytes, St.Idle), false, {}, {}, {}) }, {}, {})
                }
            }
        }
        save("models-and-ask-offer")
    }
}
