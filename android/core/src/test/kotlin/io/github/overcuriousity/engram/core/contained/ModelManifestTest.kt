package io.github.overcuriousity.engram.core.contained

import org.junit.Assert.*
import org.junit.Test

class ModelManifestTest {
    @Test fun everyEntryCanBeFetchedAndChecked() = ModelManifest.all.forEach { m ->
        assertTrue(m.name, m.url.startsWith("https://"))
        assertTrue("${m.name} is not pinned to a revision", Regex("/resolve/[0-9a-f]{40}/").containsMatchIn(m.url))
        assertTrue(m.name, Regex("[0-9a-f]{64}").matches(m.sha256))
        assertTrue(m.name, m.bytes > 1_000_000)
        assertTrue(m.name, m.licence.isNotBlank())
        assertTrue(m.name, Regex("[a-z0-9._-]+").matches(m.file))
    }

    @Test fun filesAndNamesAreDistinct() {
        assertEquals(ModelManifest.all.size, ModelManifest.all.map { it.file }.toSet().size)
        assertEquals(ModelManifest.all.size, ModelManifest.all.map { it.name }.toSet().size)
    }

    @Test fun theFirstStartSetIsTheEmbedderAndNothingElse() {
        assertEquals(listOf(Role.embed), ModelManifest.required.map { it.role })
        assertNull(ModelManifest.all.firstOrNull { it.role == Role.rerank })
    }

    @Test fun eachOfferedRoleHasOneDefault() {
        assertEquals("Qwen3.5-2B", ModelManifest.defaultFor(Role.ask)!!.name)
        assertEquals(2, ModelManifest.all.count { it.role == Role.ask })
        assertNotNull(ModelManifest.defaultFor(Role.speech))
    }
}
