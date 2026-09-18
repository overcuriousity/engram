package io.github.overcuriousity.engram.core.read

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.builtins.serializer
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonObject

/**
 * The API's shapes, as `docs/api.md` states them and as
 * `src/web/android_fixtures.rs` proves them: the fixtures these are tested
 * against are this server's own answers.
 *
 * Every field a newer server might add is ignored and every field an older one
 * might omit has a default, so neither end has to be updated first.
 */
val ApiJson = Json { ignoreUnknownKeys = true; explicitNulls = false; coerceInputValues = true }

/** Every list the API answers. `next` goes back as `after`; null means there is no more. */
@Serializable
data class Page<T>(val items: List<T> = emptyList(), val next: String? = null)

/** One search result, or one row of "worth seeing again", or one citation. */
@Serializable
data class Hit(
    @SerialName("artifact_id") val artifactId: String,
    @SerialName("corpus_id") val corpusId: String = "",
    val title: String? = null,
    val text: String = "",
    val category: String? = null,
    val tags: List<String> = emptyList(),
    /** Only loosely related to the query. Drawn as a badge in place of a rank. */
    val weak: Boolean = false,
    /** Below the point where this list's relevance falls off. What the divider is drawn from. */
    @SerialName("past_cliff") val pastCliff: Boolean = false,
    val retired: Boolean = false,
    val primed: Boolean = false,
    @SerialName("in_sitting") val inSitting: Boolean = false,
    @SerialName("model_written") val modelWritten: Boolean = false,
    @SerialName("origin_count") val originCount: Int = 0,
    @SerialName("due_in") val dueIn: String? = null,
    /** `title` is not a name of this text — a section heading, or the note's name lent to it. */
    @SerialName("borrowed_name") val borrowedName: Boolean = false,
    /** Recalled beside another hit rather than ranked: it competed for nothing and has no rank. */
    val via: String? = null,
    val reason: String? = null,
)

@Serializable
data class CorpusRow(
    val id: String,
    val origin: String = "",
    val label: String = "",
    val named: Boolean = false,
    val status: String = "",
    @SerialName("created_at") val createdAt: Long = 0,
    @SerialName("source_url") val sourceUrl: String? = null,
    @SerialName("near_dupe_of") val nearDupeOf: String? = null,
)

@Serializable
data class Span(@SerialName("start_line") val startLine: Long, @SerialName("end_line") val endLine: Long)

@Serializable
data class Chunk(
    val id: String,
    @SerialName("corpus_id") val corpusId: String? = null,
    val provenance: String = "",
    val text: String = "",
    val title: String? = null,
    val category: String? = null,
    val tags: List<String> = emptyList(),
    val caveats: List<String> = emptyList(),
    val status: String = "active",
    @SerialName("superseded_by") val supersededBy: String? = null,
    @SerialName("corpus_span") val span: Span? = null,
    @SerialName("created_at") val createdAt: Long = 0,
    @SerialName("last_verified_at") val lastVerifiedAt: Long? = null,
) {
    /** What the server's `names_its_own_text` decides: a passage and a note carry a heading that is not theirs. */
    val named: Boolean get() = !title.isNullOrBlank() && provenance != "passage" && provenance != "note"
}

@Serializable
data class CorpusDetail(
    val id: String,
    val origin: String = "",
    @SerialName("title_hint") val title: String? = null,
    @SerialName("raw_text") val text: String = "",
    val status: String = "",
    @SerialName("created_at") val createdAt: Long = 0,
    @SerialName("source_url") val sourceUrl: String? = null,
    val chunks: List<Chunk> = emptyList(),
)

@Serializable
data class SourceRef(val id: String, val title: String? = null, val origin: String = "", @SerialName("source_url") val sourceUrl: String? = null)

/**
 * A neighbour, or something this artifact has been needed alongside. [why]
 * and [corpusTitle] are only on a `seen_together` row: a neighbour is near by
 * resemblance and needs no explaining.
 */
@Serializable
data class RelatedRow(
    val id: String,
    val label: String = "",
    val named: Boolean = false,
    val snippet: String = "",
    val why: String? = null,
    @SerialName("corpus_title") val corpusTitle: String? = null,
)

/**
 * What the web pane lists beside an artifact, and where it continues. Two
 * lists and not one, because they answer different questions: what this
 * resembles, and what it has been reached for together with.
 */
@Serializable
data class Related(
    val related: List<RelatedRow> = emptyList(),
    @SerialName("seen_together") val seenTogether: List<RelatedRow> = emptyList(),
    /** The next passage of the same document, where this one stops mid-sentence. */
    @SerialName("continues_at") val continuesAt: String? = null,
) {
    val isEmpty: Boolean get() = related.isEmpty() && seenTogether.isEmpty() && continuesAt == null
}

/** One line of the source beside an artifact. [inSpan] is a line the artifact was drawn from, not context. */
@Serializable
data class SourceLine(val number: Long, val text: String = "", @SerialName("in_span") val inSpan: Boolean = false)

/**
 * The lines an artifact was drawn from, with a little context either side —
 * the source column of the web pane. [corpusId] is null for a merge, which
 * belongs to no document, and where the document is gone.
 */
@Serializable
data class SourceSlice(
    @SerialName("corpus_id") val corpusId: String? = null,
    val label: String = "",
    val lines: List<SourceLine> = emptyList(),
)

/** What the base says about itself. Only the one fact the screens draw is read. */
@Serializable
data class Status(
    /** Whether `POST /transcribe` is open: a speech model is configured. The mic is drawn only where it is. */
    val transcribe: Boolean = false,
)

/** The server flattens the chunk into the top level and sets `source` beside it; read twice, once for each. */
data class ArtifactDetail(val chunk: Chunk, val source: SourceRef?)

@Serializable
private data class SourceOnly(val source: SourceRef? = null)

fun decodeArtifact(body: String): ArtifactDetail =
    ArtifactDetail(ApiJson.decodeFromString(Chunk.serializer(), body), ApiJson.decodeFromString(SourceOnly.serializer(), body).source)

@Serializable
data class NodeSource(
    @SerialName("corpus_id") val corpusId: String,
    val label: String = "",
    @SerialName("start_line") val startLine: Long? = null,
    @SerialName("end_line") val endLine: Long? = null,
)

@Serializable
data class Node(
    val id: String,
    val label: String = "",
    val named: Boolean = false,
    val kind: String = "",
    @SerialName("created_at") val createdAt: Long = 0,
    val source: NodeSource? = null,
    val replaced: Boolean = false,
    val missing: Boolean = false,
    val children: List<Node> = emptyList(),
)

@Serializable
data class Lineage(
    val roots: List<Node> = emptyList(),
    @SerialName("also_replaced") val alsoReplaced: List<Node> = emptyList(),
    /** The walk stopped early. Said on the screen: a tree that quietly stops reads as a whole history. */
    val truncated: Boolean = false,
) {
    val isEmpty: Boolean get() = roots.isEmpty() && alsoReplaced.isEmpty()
}

@Serializable
data class Version(
    val n: Long,
    val title: String? = null,
    val text: String = "",
    val caveats: List<String> = emptyList(),
    @SerialName("created_at") val createdAt: Long = 0,
)

@Serializable
data class DayCorpus(val id: String, val label: String = "", val named: Boolean = false, val at: Long = 0, val text: String = "")

@Serializable
data class DayMoment(
    val id: String,
    @SerialName("artifact_id") val artifactId: String,
    val label: String = "",
    val named: Boolean = false,
    val kind: String = "due",
    val at: Long? = null,
    val done: Boolean = false,
    val span: String? = null,
)

@Serializable
data class Opened(val id: String, val label: String = "", val named: Boolean = false)

@Serializable
data class DaySitting(
    @SerialName("opened_at") val openedAt: Long,
    @SerialName("closed_at") val closedAt: Long,
    val query: String = "",
    val searches: Int = 0,
    val opened: List<Opened> = emptyList(),
)

@Serializable
data class Day(
    val date: String,
    val tz: String = "UTC",
    val entries: List<DayCorpus> = emptyList(),
    val captured: List<DayCorpus> = emptyList(),
    @SerialName("was_due") val wasDue: List<DayMoment> = emptyList(),
    val refers: List<DayMoment> = emptyList(),
    val sittings: List<DaySitting> = emptyList(),
) {
    val isEmpty: Boolean get() = entries.isEmpty() && captured.isEmpty() && wasDue.isEmpty() && refers.isEmpty() && sittings.isEmpty()
}

@Serializable
data class Moment(
    val id: String,
    @SerialName("artifact_id") val artifactId: String,
    val at: Long? = null,
    val rule: String? = null,
    @SerialName("snoozed_until") val snoozedUntil: Long? = null,
)

@Serializable
data class DueRow(val moment: Moment, val title: String = "", val named: Boolean = false, val opening: String = "") {
    /** When it is due as the list orders it: a snooze moves it. */
    val at: Long? get() = moment.snoozedUntil ?: moment.at
}

@Serializable
data class Offer(
    @SerialName("artifact_id") val artifactId: String,
    val label: String = "",
    val named: Boolean = false,
    val snippet: String = "",
    /** The ladder's word — pattern, similar, tentative, random. Sent back verbatim by `seen`. */
    val rung: String = "random",
    val slot: Long? = null,
    val events: Long = 0,
    val at: Long? = null,
    @SerialName("at_tz") val atTz: String? = null,
)

@Serializable
data class OfferAnswer(val offer: Offer? = null)

/** What an ask ends with: the answer the server stands behind, and what it could not vouch for. */
@Serializable
data class AskAnswer(
    val answer: String = "",
    val citations: List<Hit> = emptyList(),
    val dropped: Int = 0,
    val truncated: Boolean = false,
    val abstained: Boolean = false,
    /** Literals in the answer that no excerpt carries: the model's own, not the base's. */
    val unsupported: List<String> = emptyList(),
    @SerialName("retired_only") val retiredOnly: Boolean = false,
)

// ── Judging ──────────────────────────────────────────────────────────────────

/** One side of a duplicate pair, with enough of it to decide by. */
@Serializable
data class PairSide(
    val id: String,
    val label: String = "",
    val named: Boolean = false,
    val excerpt: String = "",
)

/**
 * One open pair, carrying only what somebody actually established about it.
 *
 * Three of these fields are there to stop a card claiming more than was found.
 * [unjudged] means the sweep filed this on a cosine score and nothing has read
 * it since, so "these two cover the same ground" is a finding nobody made —
 * the measurement is what there is to draw. [viaLink] means no cosine was ever
 * computed, so [percent] is not a similarity and must not be shown as one.
 * [mergeable] is whether the merge path would take a synthesis at all.
 *
 * Its default is false, and deliberately: a server that does not send the
 * field leaves the button out, rather than offering a press that can only come
 * back a validation error.
 */
@Serializable
data class Pair(
    val id: Long,
    val percent: Long = 0,
    @SerialName("via_link") val viaLink: Boolean = false,
    val a: PairSide,
    val b: PairSide,
    /** The judge's line, where one was written. */
    val finding: String? = null,
    val contradiction: Boolean = false,
    val vacuous: Boolean = false,
    val unjudged: Boolean = false,
    val unmergeable: Boolean = false,
    val mergeable: Boolean = false,
    @SerialName("synthesis_asked") val synthesisAsked: Boolean = false,
    /** The artifact the judge's proposal amounts to keeping, where it made one. */
    val keeps: String? = null,
)

/** One decision, however many pairs it takes to state it. */
@Serializable
data class PairCluster(val members: Int = 1, val pairs: List<Pair> = emptyList())

/**
 * The pair queue, and how many are waiting beyond it.
 *
 * Its own type rather than [Page], for the reason [SetAside] has one: the
 * queue is bounded rather than paged — `next` is always null — so the cap is
 * the only thing that can say there is more, and a cap that goes unreported
 * reads as the whole queue. [more] is the number, which is what the web page
 * says out loud; zero where nothing is waiting, and where a server too old to
 * send it said nothing.
 */
@Serializable
data class PairQueue(
    val items: List<PairCluster> = emptyList(),
    val next: String? = null,
    val more: Int = 0,
)

/** One question nothing covered. [kind] and [id] are what dismissing it names. */
@Serializable
data class GapMember(val kind: String = "", val id: String = "", val text: String = "")

/**
 * Questions the sweep found to be about one subject. [labelledBy] is `model`
 * or `terms` — whether a model named this group or its shared wording did,
 * which is the difference between a reading and a description.
 */
@Serializable
data class GapCluster(
    val label: String = "",
    @SerialName("labelled_by") val labelledBy: String = "",
    val members: List<GapMember> = emptyList(),
)

@Serializable
data class Held(val corpora: Long = 0, val artifacts: Long = 0, val segments: Long = 0, val synthesized: Long = 0)

@Serializable
data class UsedBand(val label: String = "", val count: Long = 0)

/** Null throughout where nothing was judged — never 0.00, which reads as a score. */
@Serializable
data class Retrieval(
    val judged: Long = 0,
    @SerialName("recall_at_10") val recallAt10: Double? = null,
    val mrr: Double? = null,
)

/** What the base is like. Read-only: nothing about tuning crosses. */
@Serializable
data class Insights(
    val held: Held = Held(),
    val used: List<UsedBand> = emptyList(),
    /** Absent where no searches are recorded. */
    val retrieval: Retrieval? = null,
)

/** What the base put beside a set-aside row: a merge's sources, a near-duplicate's winner. */
@Serializable
data class Beside(
    val id: String = "",
    @SerialName("corpus_id") val corpusId: String = "",
    val label: String = "",
    val named: Boolean = false,
)

/**
 * One thing the base did on its own, or is waiting to be told about.
 *
 * [kind] is the whole of what says which answers the row admits — see
 * [actionsFor]. [subjectId] is what those answers name, and which thing that
 * is depends on the kind: a corpus for `parked`, the artifact for the rest.
 */
@Serializable
data class SetAsideRow(
    val kind: String = "",
    @SerialName("subject_id") val subjectId: String = "",
    /** The artifact to open. Null for a parked capture, which is a corpus. */
    @SerialName("artifact_id") val artifactId: String? = null,
    val label: String = "",
    val named: Boolean = false,
    /** What tells two rows with one label apart. Empty where nothing does. */
    val subtitle: String = "",
    val why: String = "",
    val beside: List<Beside> = emptyList(),
    /** The one thing about this row that is not simply reversible. */
    val caveat: String? = null,
)

/** The undo list, and whether any cap bit. */
@Serializable
data class SetAside(
    val items: List<SetAsideRow> = emptyList(),
    val next: String? = null,
    val capped: Boolean = false,
)

/** An answer a set-aside row admits. Each is a route; none of them is a rendering. */
enum class SetAsideAction { Verify, Deprecate, Reactivate, UndoMerge, ResolveReplace, ResolveKeepBoth, ResolveDiscard }

/**
 * Which answers a row admits, from its `kind` and nothing else — no reading of
 * its wording, no guess from what is beside it. A kind this build has never
 * heard of admits none, which is what lets the server grow a seventh without
 * breaking an older app.
 *
 * `hidden` arrives from two places — an artifact superseded by a near-duplicate
 * and one deprecated by hand — and the kind does not say which. It does not
 * have to: `reactivate` answers either, routing a supersession through the
 * server's own unsupersede, so the one button here can never be a press that
 * comes back refused.
 */
fun actionsFor(kind: String): List<SetAsideAction> = when (kind) {
    "unverified" -> listOf(SetAsideAction.Verify, SetAsideAction.Deprecate)
    "merged" -> listOf(SetAsideAction.UndoMerge)
    "generated" -> listOf(SetAsideAction.Deprecate)
    "hidden", "buried" -> listOf(SetAsideAction.Reactivate)
    "parked" -> listOf(SetAsideAction.ResolveReplace, SetAsideAction.ResolveKeepBoth, SetAsideAction.ResolveDiscard)
    else -> emptyList()
}

/** How each body is read. */
object Decode {
    private fun <T> page(item: kotlinx.serialization.KSerializer<T>): (String) -> Page<T> =
        { ApiJson.decodeFromString(Page.serializer(item), it) }

    val hits = page(Hit.serializer())
    val corpora = page(CorpusRow.serializer())
    val versions = page(Version.serializer())
    val due = page(DueRow.serializer())
    val corpus: (String) -> CorpusDetail = { ApiJson.decodeFromString(CorpusDetail.serializer(), it) }
    val lineage: (String) -> Lineage = { ApiJson.decodeFromString(Lineage.serializer(), it) }
    val day: (String) -> Day = { ApiJson.decodeFromString(Day.serializer(), it) }
    val offer: (String) -> OfferAnswer = { ApiJson.decodeFromString(OfferAnswer.serializer(), it) }
    val artifact: (String) -> ArtifactDetail = ::decodeArtifact
    val related: (String) -> Related = { ApiJson.decodeFromString(Related.serializer(), it) }
    val source: (String) -> SourceSlice = { ApiJson.decodeFromString(SourceSlice.serializer(), it) }
    val status: (String) -> Status = { ApiJson.decodeFromString(Status.serializer(), it) }
    val pairs: (String) -> PairQueue = { ApiJson.decodeFromString(PairQueue.serializer(), it) }
    val gaps = page(GapCluster.serializer())
    val insights: (String) -> Insights = { ApiJson.decodeFromString(Insights.serializer(), it) }
    val setAside: (String) -> SetAside = { ApiJson.decodeFromString(SetAside.serializer(), it) }
}

/** The reads, by name. One place knows a path; a screen knows what it wants. */
object Api {
    fun search(q: String) = Request("/api/v1/search", mapOf("q" to q))
    fun resurface() = Request("/api/v1/resurface", mapOf("limit" to "5"))
    fun due() = Request("/api/v1/moments", mapOf("kind" to "due"))
    fun corpora(after: String?) = Request("/api/v1/corpora", mapOf("limit" to "50", "after" to after))
    fun corpus(id: String) = Request("/api/v1/corpora/$id")
    fun artifact(id: String) = Request("/api/v1/artifacts/$id")
    fun lineage(id: String) = Request("/api/v1/artifacts/$id/lineage")
    fun versions(id: String) = Request("/api/v1/artifacts/$id/versions")
    fun related(id: String) = Request("/api/v1/artifacts/$id/related")
    fun source(id: String) = Request("/api/v1/artifacts/$id/source")
    fun status() = Request("/api/v1/status")
    fun day(date: String, tz: String) = Request("/api/v1/days/$date", mapOf("tz" to tz))
    fun pairs() = Request("/api/v1/pairs")
    fun gaps() = Request("/api/v1/gaps")
    fun insights() = Request("/api/v1/insights")
    fun setAside() = Request("/api/v1/insights/set-aside")

    const val CONTEXT = "/api/v1/context"
    const val SEEN = "/api/v1/context/seen"
    fun seen(o: Offer): String = JsonObject(
        buildMap {
            put("artifact_id", kotlinx.serialization.json.JsonPrimitive(o.artifactId))
            put("rung", kotlinx.serialization.json.JsonPrimitive(o.rung))
            if (o.slot != null) put("slot", kotlinx.serialization.json.JsonPrimitive(o.slot))
        },
    ).toString()
}
