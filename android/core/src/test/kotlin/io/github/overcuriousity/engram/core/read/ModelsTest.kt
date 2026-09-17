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
        // Counted by property, not by total: the fixture base grows whenever
        // the server needs another shape in it, and a test that breaks on its
        // arithmetic is testing the fixture rather than the model.
        val rows = Decode.corpora(fixture("corpora.json")).items
        assertTrue(rows.isNotEmpty())
        val named = rows.single { it.label == "Qdrant notes" }
        assertTrue("a title the base holds is a name", named.named)
        assertTrue("every row is drawable", rows.all { it.label.isNotEmpty() })
        assertTrue("a capture with no title yet stands in with its opening", rows.any { !it.named })
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
        assertTrue(d.captured.any { it.label == "Qdrant notes" && it.named })
        assertTrue(d.captured.all { it.at > 0 })
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

    @Test fun aPairSaysOnlyWhatSomebodyEstablished() {
        val cluster = Decode.pairs(fixture("pairs.json")).items.first()
        assertTrue("a cluster says how many artifacts the one question is about", cluster.members >= 2)
        val p = cluster.pairs.first()
        // The sweep filed this one on a score and nothing has read it since,
        // so there is no finding to print over it.
        assertTrue(p.unjudged)
        assertNull(p.finding)
        assertFalse(p.viaLink)
        assertTrue("a measurement there is one of", p.percent > 0)
        assertTrue(p.a.label.isNotEmpty() && p.a.excerpt.isNotEmpty())
        assertTrue(p.b.id != p.a.id)
        assertNull("nobody proposed a side", p.keeps)
    }

    /**
     * The queue is capped and never paged, so the cap is the only thing that
     * can say there is more of it. Dropped, five answered pairs read as the
     * whole backlog.
     */
    @Test fun thePairQueueSaysHowManyAreWaitingBeyondIt() {
        assertEquals(0, Decode.pairs(fixture("pairs.json")).more)
        assertEquals(2, Decode.pairs("""{"items":[],"next":null,"more":2}""").more)
        // A server too old to send it says nothing, rather than a guess.
        assertEquals(0, Decode.pairs("""{"items":[]}""").more)
    }

    @Test fun mergeableIsFalseUnlessTheServerSaysOtherwise() {
        // The default a missing field falls to decides whether a button is
        // drawn whose press can only come back a validation error.
        val p = ApiJson.decodeFromString(
            Pair.serializer(),
            """{"id":1,"a":{"id":"a"},"b":{"id":"b"}}""",
        )
        assertFalse(p.mergeable)
        assertFalse(p.unjudged)
    }

    @Test fun aGapClusterSaysWhoNamedIt() {
        val g = Decode.gaps(fixture("gaps.json")).items.first()
        assertTrue(g.label.isNotEmpty())
        assertTrue("a name from the words, or from a model", g.labelledBy in setOf("terms", "model"))
        val m = g.members.first()
        assertTrue(m.kind.isNotEmpty() && m.id.isNotEmpty() && m.text.isNotEmpty())
    }

    @Test fun retrievalIsAbsentRatherThanZeroWhereNothingWasJudged() {
        val i = Decode.insights(fixture("insights.json"))
        assertTrue(i.held.artifacts > 0)
        assertTrue(i.used.any { it.label == "never reached" })
        assertEquals(0L, i.retrieval?.judged)
        assertNull("not 0.00, which would read as a score", i.retrieval?.recallAt10)
    }

    @Test fun aSetAsideRowCarriesItsKindAndTheIdAnAnswerNames() {
        val rows = Decode.setAside(fixture("set_aside.json")).items
        val merged = rows.first { it.kind == "merged" }
        assertEquals(merged.artifactId, merged.subjectId)
        assertTrue(merged.why.isNotEmpty())
        assertTrue("what it was written from", merged.beside.isNotEmpty())
        assertNull(merged.caveat)
        val hidden = rows.first { it.kind == "hidden" }
        assertTrue(hidden.beside.single().label.isNotEmpty())
    }

    @Test fun aKindThisAppHasNeverHeardOfDrawsNoButtons() {
        assertEquals(emptyList<SetAsideAction>(), actionsFor("something-the-server-grew"))
        // Both sorts of `hidden`, and a buried one, come back the same way,
        // and reactivate is the call that answers either.
        assertEquals(listOf(SetAsideAction.Reactivate), actionsFor("hidden"))
        assertEquals(listOf(SetAsideAction.Reactivate), actionsFor("buried"))
        assertEquals(listOf(SetAsideAction.UndoMerge), actionsFor("merged"))
        assertEquals(listOf(SetAsideAction.Deprecate), actionsFor("generated"))
        assertEquals(listOf(SetAsideAction.Verify, SetAsideAction.Deprecate), actionsFor("unverified"))
        assertEquals(3, actionsFor("parked").size)
    }
}
