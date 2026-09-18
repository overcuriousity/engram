package io.github.overcuriousity.engram.core

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import java.io.File
import kotlin.io.path.createTempDirectory

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [35])
class ModeTest {
    private val prefs = ApplicationProvider.getApplicationContext<Context>().getSharedPreferences("m", Context.MODE_PRIVATE)
    private val files = createTempDirectory("files").toFile()

    @Test fun nothingIsChosenUntilSomethingIs() {
        assertNull(ModeStore(prefs).chosen)
        ModeStore(prefs).chosen = Mode.contained
        assertEquals(Mode.contained, ModeStore(prefs).chosen)
    }

    @Test fun aWordThisBuildDoesNotKnowIsNoChoice() {
        prefs.edit().putString("mode", "orbital").apply()
        assertNull(ModeStore(prefs).chosen)
    }

    @Test fun serverModeStaysWhereItAlwaysWas() {
        val s = ModeState.of(Mode.server, files)
        assertEquals("engram.db", s.dbName)
        assertEquals(File(files, "outbox"), s.outbox)
        assertNull(s.core)
    }

    @Test fun containedModeSharesNoPathWithIt() {
        val s = ModeState.of(Mode.contained, files)
        assertEquals("contained.db", s.dbName)
        assertEquals(File(files, "contained/outbox"), s.outbox)
        assertEquals(File(files, "contained/core"), s.core)
    }

    @Test fun onlyAWholeModelIsNamed() {
        val s = ModeState.of(Mode.contained, files)
        assertEquals(io.github.overcuriousity.engram.core.contained.Models(), s.models())
        s.models!!.mkdirs()
        val embed = io.github.overcuriousity.engram.core.contained.ModelManifest.required.single()
        val f = File(s.models, embed.file)
        java.io.RandomAccessFile(f, "rw").use { it.setLength(embed.bytes - 1) }
        assertNull(s.models().embed)
        java.io.RandomAccessFile(f, "rw").use { it.setLength(embed.bytes) }
        assertEquals(f.path, s.models().embed)
        assertNull(s.models().ask)
        // A download in flight is not a model.
        File(s.models, embed.file + ".part").writeText("x")
        assertEquals(f.path, s.models().embed)
    }

    @Test fun askIsOnTheDeviceUntilSomebodySaysOtherwise() {
        assertEquals(AskVia.device, ModeStore(prefs).ask)
        ModeStore(prefs).ask = AskVia.off
        assertEquals(AskVia.off, ModeStore(prefs).ask)
    }
}
