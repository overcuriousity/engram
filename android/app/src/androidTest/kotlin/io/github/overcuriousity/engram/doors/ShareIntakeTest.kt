package io.github.overcuriousity.engram.doors

import androidx.core.content.FileProvider
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import io.github.overcuriousity.engram.App
import io.github.overcuriousity.engram.core.db.Kind
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File

@RunWith(AndroidJUnit4::class)
class ShareIntakeTest {
    @Test fun aContentUriIsCopiedIntoTheOutbox() = runBlocking {
        val app = ApplicationProvider.getApplicationContext<App>()
        val src = File(app.cacheDir, "shared.txt").apply { writeText("shared bytes") }
        val uri = FileProvider.getUriForFile(app, "${app.packageName}.files", src)
        val id = Intake.uris(app.engram, listOf(uri), "t", null)
        src.delete() // the sender's file is gone; ours must not be
        val row = app.engram.outbox.rows.first().first { it.id == id }
        assertEquals(Kind.capture_files, row.kind)
        val f = app.engram.outbox.filesOf(id).single()
        assertEquals("shared bytes", File(f.path).readText())
    }
}
