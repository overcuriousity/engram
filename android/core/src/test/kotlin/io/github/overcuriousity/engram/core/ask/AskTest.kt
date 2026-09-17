package io.github.overcuriousity.engram.core.ask

import androidx.test.core.app.ApplicationProvider
import io.github.overcuriousity.engram.core.Connection
import io.github.overcuriousity.engram.core.Transport
import io.github.overcuriousity.engram.core.db.Db
import io.github.overcuriousity.engram.core.read.AskAnswer
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.toList
import kotlinx.coroutines.test.runTest
import mockwebserver3.MockResponse
import mockwebserver3.MockWebServer
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [35])
class AskTest {
    private val start = AskState("why")

    @Test fun tokensGrowTheDraftAndTheFirstOneStartsTheWriting() {
        val s = listOf(AskFrame.Token("Run "), AskFrame.Token("it.")).fold(start, ::reduce)
        assertEquals("Run it.", s.draft)
        assertEquals(Phase.Writing, s.phase)
    }

    @Test fun retrievalSaysHowMuchWasShownAndLeftOut() {
        val s = reduce(start, AskFrame.Retrieved(shown = 4, dropped = 9))
        assertEquals(Phase.Retrieving, s.phase)
        assertEquals(4, s.shown); assertEquals(9, s.dropped)
    }

    @Test fun theFinishedAnswerReplacesTheDraft() {
        val answer = AskAnswer(answer = "The answer the server stands behind.", unsupported = listOf("x"))
        val s = reduce(reduce(start, AskFrame.Token("a draft the app assembled")), AskFrame.Done(answer))
        assertEquals(Phase.Done, s.phase)
        assertEquals(answer, s.answer)
        // What is drawn from here on is `answer`, not `draft`.
        assertEquals("The answer the server stands behind.", s.text)
    }

    @Test fun aFailureKeepsWhatWasWrittenSoFarAndSaysWhy() {
        val s = reduce(reduce(start, AskFrame.Token("half")), AskFrame.Failed("inference[ask] busy"))
        assertEquals(Phase.Failed, s.phase)
        assertEquals("inference[ask] busy", s.error)
        assertEquals("half", s.text)
    }

    @Test fun framesAreParsedByNameAndAnUnknownOneIsNothing() {
        assertEquals(AskFrame.Token("a"), parseFrame("token", """{"text":"a"}"""))
        assertEquals(AskFrame.Retrieved(3, 1), parseFrame("retrieved", """{"round":1,"retrieved":4,"shown":3,"dropped":1,"cliff_at":null}"""))
        assertEquals(AskFrame.Failed("no"), parseFrame("error", """{"error":"no"}"""))
        assertEquals(AskFrame.Needs(listOf("q1")), parseFrame("needs", """{"queries":["q1"]}"""))
        assertNull(parseFrame("something_new", "{}"))
        assertNull(parseFrame("token", "not json"))
        val done = parseFrame("done", javaClass.getResource("/api/ask_done.json")!!.readText()) as AskFrame.Done
        assertEquals(listOf("qdrant-cli index create"), done.answer.unsupported)
    }

    // ── What the badge is drawn from ────────────────────────────────────────

    @Test fun anAnswerWithNothingUnsupportedIsOnePlainRun() {
        assertEquals(listOf(Run("all of it", false)), annotate("all of it", emptyList()))
    }

    @Test fun everyOccurrenceIsMarked() {
        assertEquals(
            listOf(Run("run ", false), Run("rm -rf", true), Run(" then ", false), Run("rm -rf", true)),
            annotate("run rm -rf then rm -rf", listOf("rm -rf")),
        )
    }

    @Test fun aLiteralInsideAnotherIsMarkedOnceAsTheLongerOne() {
        assertEquals(
            listOf(Run("see ", false), Run("/etc/engram/config.toml", true), Run(" and ", false), Run("/etc", true)),
            annotate("see /etc/engram/config.toml and /etc", listOf("/etc", "/etc/engram/config.toml")),
        )
    }

    @Test fun aLiteralTheAnswerDoesNotContainMarksNothing() {
        assertEquals(listOf(Run("plain", false)), annotate("plain", listOf("absent", "")))
    }

    // ── The runner ──────────────────────────────────────────────────────────

    private val server = MockWebServer()
    private val db = Db.inMemory(ApplicationProvider.getApplicationContext())
    private fun ask(): Ask {
        val origin = server.url("/").toString().trimEnd('/')
        return Ask({ Transport(Connection(origin, "engram_tok", null, "0.1.0", "dev"), "ua") }, db.askedDao()) { 7L }
    }

    @Test fun aStreamedAnswerEndsDoneAndIsKeptForReadingLater() = runTest {
        server.start()
        val done = javaClass.getResource("/api/ask_done.json")!!.readText().replace("\n", "")
        server.enqueue(
            MockResponse.Builder().code(200).addHeader("Content-Type", "text/event-stream")
                .body("event: token\ndata: {\"text\":\"Run\"}\n\nevent: done\ndata: $done\n\n").build(),
        )
        val a = ask()
        val states = a.run("how do I index").toList()
        assertEquals(Phase.Retrieving, states.first().phase)
        assertEquals("Run", states[1].draft)
        assertEquals(Phase.Done, states.last().phase)
        assertEquals("""{"q":"how do I index"}""", server.takeRequest().body!!.utf8())

        val kept = a.history.first().single()
        assertEquals("how do I index", kept.question)
        assertEquals(states.last().answer, a.kept("how do I index")?.answer)
        server.close()
    }

    @Test fun anUnreachableServerIsSaidAndNothingIsKept() = runTest {
        server.start(); val a = ask(); server.close()
        val last = a.run("q").toList().last()
        assertEquals(Phase.Failed, last.phase)
        assertEquals("Server unreachable", last.error)
        assertTrue(a.history.first().isEmpty())
    }

    @Test fun aServerThatSaysNoIsQuotedInItsOwnWords() = runTest {
        server.start()
        server.enqueue(MockResponse(code = 503, body = """{"error":"inference[ask] busy: HTTP 429"}"""))
        assertEquals("inference[ask] busy: HTTP 429", ask().run("q").toList().last().error)
        server.close()
    }

    @Test fun aStreamThatEndsWithoutAnAnswerIsAFailureNotAHang() = runTest {
        server.start()
        server.enqueue(MockResponse.Builder().code(200).body("event: token\ndata: {\"text\":\"half\"}\n\n").build())
        val last = ask().run("q").toList().last()
        assertEquals(Phase.Failed, last.phase)
        assertEquals("half", last.text)
        server.close()
    }
}
