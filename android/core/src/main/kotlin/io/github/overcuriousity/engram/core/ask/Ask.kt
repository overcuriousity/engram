package io.github.overcuriousity.engram.core.ask

import io.github.overcuriousity.engram.core.PinMismatch
import io.github.overcuriousity.engram.core.Refused
import io.github.overcuriousity.engram.core.Transport
import io.github.overcuriousity.engram.core.db.AskedDao
import io.github.overcuriousity.engram.core.db.AskedRow
import io.github.overcuriousity.engram.core.read.ApiJson
import io.github.overcuriousity.engram.core.read.AskAnswer
import io.github.overcuriousity.engram.core.read.Hit
import io.github.overcuriousity.engram.core.read.ServerReader
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.channelFlow
import kotlinx.coroutines.flow.emptyFlow
import kotlinx.coroutines.flow.map
import kotlinx.serialization.builtins.ListSerializer
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.int
import kotlinx.serialization.json.jsonArray
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import java.io.IOException

/*
 * Ask is the one screen that is not a read. `POST /api/v1/ask/stream` answers
 * as a stream of named frames, and the part worth having — which commands and
 * paths in the answer no excerpt carries — arrives with the last of them. So
 * the screen draws partial text while it grows and then draws the whole answer
 * again, annotated. That is a state machine, and it is written as one: frames
 * in, state out, no Android type and no clock, which is what lets it be tested
 * to the last branch on the JVM.
 */

sealed interface AskFrame {
    data class Retrieved(val shown: Int, val dropped: Int) : AskFrame
    /** The second round: what the model said was still missing, as the queries it named. */
    data class Needs(val queries: List<String>) : AskFrame
    data class Citations(val hits: List<Hit>) : AskFrame
    data class Reasoning(val text: String) : AskFrame
    data class Token(val text: String) : AskFrame
    data class Done(val answer: AskAnswer) : AskFrame
    data class Failed(val message: String) : AskFrame
}

enum class Phase { Retrieving, Writing, Done, Failed }

data class AskState(
    val question: String,
    val phase: Phase = Phase.Retrieving,
    /** The tokens so far. Drawn only until `answer` arrives. */
    val draft: String = "",
    val citations: List<Hit> = emptyList(),
    val shown: Int? = null,
    val dropped: Int? = null,
    val needs: List<String> = emptyList(),
    val answer: AskAnswer? = null,
    val error: String? = null,
) {
    /**
     * What to draw. Once the answer is here it *replaces* the draft: what ends
     * up on screen is the answer the server stands behind, not a concatenation
     * this app assembled from frames.
     */
    val text: String get() = answer?.answer ?: draft
}

fun reduce(s: AskState, f: AskFrame): AskState = when (f) {
    is AskFrame.Retrieved -> s.copy(shown = f.shown, dropped = f.dropped)
    is AskFrame.Needs -> s.copy(needs = f.queries)
    is AskFrame.Citations -> s.copy(citations = f.hits)
    is AskFrame.Reasoning -> s
    is AskFrame.Token -> s.copy(phase = Phase.Writing, draft = s.draft + f.text)
    is AskFrame.Done -> s.copy(phase = Phase.Done, answer = f.answer, citations = f.answer.citations.ifEmpty { s.citations })
    is AskFrame.Failed -> s.copy(phase = Phase.Failed, error = f.message)
}

/** One frame off the wire. A name this app does not know, or data it cannot read, is nothing — never a crash. */
fun parseFrame(event: String, data: String): AskFrame? = runCatching {
    val o: JsonObject = ApiJson.parseToJsonElement(data).jsonObject
    when (event) {
        "retrieved" -> AskFrame.Retrieved(o["shown"]!!.jsonPrimitive.int, o["dropped"]!!.jsonPrimitive.int)
        "needs" -> AskFrame.Needs(o["queries"]!!.jsonArray.map { it.jsonPrimitive.content })
        "citations" -> AskFrame.Citations(ApiJson.decodeFromJsonElement(ListSerializer(Hit.serializer()), o["hits"]!!))
        "reasoning" -> AskFrame.Reasoning(o["text"]!!.jsonPrimitive.content)
        "token" -> AskFrame.Token(o["text"]!!.jsonPrimitive.content)
        "done" -> AskFrame.Done(ApiJson.decodeFromJsonElement(AskAnswer.serializer(), o))
        "error" -> AskFrame.Failed(o["error"]?.jsonPrimitive?.contentOrNull ?: "the answer failed")
        else -> null
    }
}.getOrNull()

/** A stretch of the answer, and whether it is a literal no excerpt carries. */
data class Run(val text: String, val unsupported: Boolean)

/**
 * The answer cut into runs, with every occurrence of every unsupported literal
 * marked. Longest literal first and never overlapping, so `/etc` inside
 * `/etc/engram/config.toml` is part of the longer one and not a mark within a
 * mark. Exact matching on strings the server itself lifted out of this text.
 */
fun annotate(answer: String, unsupported: List<String>): List<Run> {
    val marked = BooleanArray(answer.length)
    for (lit in unsupported.filter { it.isNotEmpty() }.distinct().sortedByDescending { it.length }) {
        var from = answer.indexOf(lit)
        while (from >= 0) {
            val to = from + lit.length
            if ((from until to).none { marked[it] }) (from until to).forEach { marked[it] = true }
            from = answer.indexOf(lit, from + 1)
        }
    }
    val out = mutableListOf<Run>()
    var i = 0
    while (i < answer.length) {
        var j = i
        while (j < answer.length && marked[j] == marked[i]) j++
        out += Run(answer.substring(i, j), marked[i])
        i = j
    }
    return out
}

/** An answer kept from earlier, with when it was given. */
data class Kept(val question: String, val answer: AskAnswer, val askedAt: Long)

class Ask internal constructor(
    private val transport: () -> Transport?,
    private val dao: AskedDao,
    private val onRefused: () -> Unit = {},
    private val onPinMismatch: (PinMismatch) -> Unit = {},
    private val clock: () -> Long = System::currentTimeMillis,
) {
    internal constructor(transport: () -> Transport?, dao: AskedDao, clock: () -> Long) : this(transport, dao, {}, {}, clock)

    /**
     * Ask, and emit every state on the way. Cancelling the collection closes
     * the call: an ask nobody is reading goes on retrieving and prompting on
     * the server, holding the lane the next ask needs.
     */
    fun run(question: String): Flow<AskState> = channelFlow {
        var s = AskState(question)
        send(s)
        val t = transport()
        if (t == null) {
            send(s.copy(phase = Phase.Failed, error = "Not paired"))
            return@channelFlow
        }
        val body = JsonObject(mapOf("q" to JsonPrimitive(question))).toString()
        val failed: String? = try {
            val a = t.stream("/api/v1/ask/stream", emptyMap(), body) { event, data ->
                val f = parseFrame(event, data) ?: return@stream
                s = reduce(s, f)
                send(s)
            }
            when {
                a.status !in 200..299 -> ServerReader.said(a.body, a.status)
                // The socket closed before `done` or `error`: a proxy timeout,
                // a server restart. Not an answer, and not something to wait on.
                s.phase != Phase.Done && s.phase != Phase.Failed -> "The answer stopped early"
                else -> null
            }
        } catch (e: Refused) {
            onRefused(); "Unpaired on the server"
        } catch (e: PinMismatch) {
            onPinMismatch(e); "Certificate changed"
        } catch (e: IOException) {
            "Server unreachable"
        }
        if (failed != null) send(s.copy(phase = Phase.Failed, error = failed))
        val answer = s.answer
        if (failed == null && answer != null) {
            dao.put(AskedRow(question, t.connection.origin, ApiJson.encodeToString(AskAnswer.serializer(), answer), clock()))
        }
    }

    /** Earlier questions on this server, newest first. Readable without it. */
    val history: Flow<List<Kept>>
        get() {
            val origin = transport()?.connection?.origin ?: return emptyFlow()
            return dao.recent(origin).map { rows -> rows.mapNotNull(::kept) }
        }

    suspend fun kept(question: String): Kept? {
        val origin = transport()?.connection?.origin ?: return null
        return dao.get(question, origin)?.let(::kept)
    }

    private fun kept(r: AskedRow): Kept? =
        runCatching { Kept(r.question, ApiJson.decodeFromString(AskAnswer.serializer(), r.body), r.askedAt) }.getOrNull()
}
