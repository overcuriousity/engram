package io.github.overcuriousity.engram.core.read

import org.junit.Assert.*
import org.junit.Test

/**
 * Decoded from `resources/api/`, which `src/web/android_fixtures.rs` writes
 * from the real routes and checks on every server test run. Nothing here is a
 * hand-written guess at what the server sends.
 */
class ModelsTest {
    private fun fixture(name: String) = javaClass.getResource("/api/$name")!!.readText()

    @Test fun aLooseHitPastTheCliffKeepsBothFactsAndASureOneHasNeither() {
        val page = Decode.hits(fixture("search.json"))
        assertNull(page.next)
        val (sure, loose) = page.items
        assertFalse(sure.weak); assertFalse(sure.pastCliff)
        assertTrue(loose.weak); assertTrue(loose.pastCliff)
        assertEquals("art-loose", loose.artifactId)
    }

    @Test fun aLibraryRowIsALabelThatSaysWhetherItIsAName() {
        val rows = Decode.corpora(fixture("corpora.json")).items
        assertEquals(3, rows.size)
        val named = rows.single { it.named }
        assertEquals("Qdrant notes", named.label)
        assertTrue(rows.filter { !it.named }.all { it.label.isNotEmpty() })
        assertTrue(named.createdAt > 0)
    }

    @Test fun aCorpusCarriesItsTextAndItsArtifacts() {
        val c = Decode.corpus(fixture("corpus.json"))
        assertEquals("Qdrant notes", c.title)
        assertTrue(c.text.startsWith("Filters narrow"))
        assertTrue(c.chunks.isNotEmpty())
        assertEquals(1L, c.chunks.first { it.span != null }.span!!.startLine)
    }

    @Test fun anArtifactIsItsChunkAndWhereItCameFrom() {
        val a = decodeArtifact(fixture("artifact.json"))
        assertEquals("Payload filters 0", a.chunk.title)
        assertTrue(a.chunk.named)
        assertEquals(listOf("qdrant"), a.chunk.tags)
        assertEquals("Qdrant notes", a.source?.title)
    }

    @Test fun aTreeNestsAndALeafKnowsItsLines() {
        val l = Decode.lineage(fixture("lineage.json"))
        assertFalse(l.truncated)
        assertFalse(l.isEmpty)
        val leaf = l.roots.first()
        assertEquals("captured", leaf.kind)
        assertEquals(1L, leaf.source?.startLine)
    }

    @Test fun versionsComeOldestFirst() {
        val v = Decode.versions(fixture("versions.json")).items
        assertEquals(1L, v.single().n)
        assertEquals("Both, merged.", v.single().text)
    }

    @Test fun aDayHasItsFiveSections() {
        val d = Decode.day(fixture("day.json"))
        assertEquals("UTC", d.tz)
        assertEquals("Long day.", d.entries.single().text)
        assertEquals(2, d.captured.size)
        assertEquals("due", d.wasDue.single().kind)
        assertEquals("today", d.refers.single().span)
        assertEquals("payload filter", d.sittings.single().query)
        assertFalse(d.isEmpty)
    }

    @Test fun aDueRowIsDueWhenItsSnoozeSaysSo() {
        val row = Decode.due(fixture("moments.json")).items.single()
        assertEquals(row.moment.at, row.at)
        assertEquals(99L, row.copy(moment = row.moment.copy(snoozedUntil = 99)).at)
    }

    @Test fun anOfferSendsItsRungBackVerbatim() {
        val o = Decode.offer(fixture("offer.json")).offer!!
        assertEquals("pattern", o.rung)
        assertEquals("""{"artifact_id":"${o.artifactId}","rung":"pattern","slot":3}""", Api.seen(o))
        assertNull(Decode.offer("""{"offer":null}""").offer)
    }

    @Test fun anAnswerNamesWhatItCouldNotVouchFor() {
        val a = ApiJson.decodeFromString(AskAnswer.serializer(), fixture("ask_done.json"))
        assertEquals(listOf("qdrant-cli index create"), a.unsupported)
        assertEquals(2, a.citations.size)
        assertEquals(2, a.dropped)
    }

    @Test fun aNewerServersExtraKeysAndAnOlderOnesMissingOnesAreBothFine() {
        val h = ApiJson.decodeFromString(Hit.serializer(), """{"artifact_id":"a","something_new":{"x":1}}""")
        assertEquals("a", h.artifactId)
        assertFalse(h.pastCliff)
    }
}
